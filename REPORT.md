# Feature 7 — File > Compact

Branch `feat/compact` in `C:/Users/Error/projects/ferrix-compact`.

Bakes the `.fxedits` overlay back into the `.ferrix` columnar cache so the
sidecar can be retired, instead of being reapplied on every open.

## Gates

All three green on the final commit:

| Gate | Result |
|---|---|
| `cargo test --workspace` | **923 passed, 0 failed** (was 909; +14) |
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |

## What was implemented

### `crates/ferrix-io/src/compact.rs` (new)

`compact_cache(cache, overlay, progress, should_cancel)` streams the existing
cache column by column, applying the overlay as it goes, and commits the
result atomically.

**Shares convert.rs's writer, does not duplicate it.** `Spill` (the per-column
buffered writer) and `assemble` (the `.ferrix` layout stitcher) were made
`pub(crate)` and are used as-is. The only addition to the writer is
`Spill::push_error`, which the CSV converter never needed — a CSV cannot spell
`#DIV/0!` as a typed value — but which a baked-in formula result requires. It
writes the tag and the stable error code exactly as `MappedSheet::get` reads
it back.

`assemble` now `fsync`s before returning, so a caller that renames has a
durable file to rename.

**Memory.** Peak is one column stripe. The resident, non-reclaimable set is:
three 1 MiB buffered writers for the column in flight, the string arena
(bounded by *distinct* strings), and the overlay's edits grouped by column
(bounded by the *edit count*). The source is read through the existing mmap, so
its pages are file-backed page cache rather than heap. Nothing scales with row
count; the sheet is never materialised.

**Atomicity.** The ordering is: write `<cache>.compacting` (a sibling, so the
rename stays on one filesystem) → fsync → rename over the cache → *only then*
delete the sidecar and its autosave. Crash before the rename and the original
cache and sidecar are both untouched and openable; the stale `.compacting` file
is inert, because the loader only ever opens `<name>.ferrix`. Crash between the
rename and the delete and the cache is the new one while the sidecar still
exists — and the sidecar's fingerprint no longer matches the rewritten base, so
it is *rejected* loudly rather than applied a second time. Double-application is
the failure this ordering is chosen to prevent.

**Staleness fingerprint.** `fingerprint_after()` re-derives the fingerprint
against the file that now exists. Verified by test that
`cache_is_fresh(source, cache)` still holds after a compact, so a later open
does not think the cache is stale and reconvert.

**Formulas.** A columnar cache stores values, not expressions, so a formula's
*cached result* is baked in and the formula itself is carried forward in a
fresh, much smaller sidecar written against the new fingerprint. `=SUM(A1:A10)`
stays a formula across a compact rather than silently freezing into a number.
An all-literals overlay — the ordinary case — ends with no sidecar at all.

### `crates/ferrix-ui` — menu entry and progress modal

- **File menu** (new): Open / Save edits / **Compact…** / Export CSV. Compact
  is enabled only when there is a columnar cache, edits to bake, and no other
  long job running; the disabled tooltip says which of those is missing.
- **Progress modal** with a per-column bar and a working Cancel (also Escape).
  Deliberately modal rather than a toolbar spinner like export: an export only
  reads, while a compact is rewriting the file the grid reads from. The modal
  states plainly that the existing file is untouched until it finishes.
- **The live mapping is dropped before the worker starts and re-adopted after.**
  On Windows an open mapping locks the file and the rename fails; on Unix it
  would succeed but leave the process reading a deleted inode. On failure or
  cancel the original cache is re-mapped and the user is exactly where they
  were, edits intact.
- **Undo history is cleared on compact**, via the same `save_committed()` that
  save uses, and the count is reported in the status bar rather than dropped
  silently.
- The fingerprint is re-anchored after the commit, so the next save is not
  rejected as stale.

### `crates/ferrix-bench/src/check_compact.rs` (new), beside `check_export.rs`

Registered as the `check-compact` binary. Builds a cache, edits 100 cells
spread across the whole file (alternating text and numbers so both writer
sections are exercised), writes a sidecar, compacts, and verifies. The original
cache is copied aside and the two are compared by **streaming both mappings a
cell at a time** — O(1) heap — rather than snapshotting, so every cell is
checked at any file size instead of sampling. `--peak` measures memory and
skips the full comparison, because mapping a second copy of the file would
confound the very working-set number being measured.

## Tests

14 new tests. The three required scenarios:

**1. Edit 100 cells over a large cache, compact, reopen.** Covered at two
levels. `check-compact` on a 300,000-row / 8-column fixture: sidecar gone; all
100 edited cells show their edited value; **all 2,399,900 unedited cells
identical, compared cell by cell**; every row still at its original index.
The unit test `every_unedited_cell_is_identical_and_rows_keep_their_order`
makes the same assertions in-process.

**Row order is checked per row, not by a checksum.** Column 0 is the row's
identity in the generated data and must appear at the same index; the
comparison walks the original and the new mapping in lockstep and reports the
exact `(row, col)` of any mismatch. A total was deliberately not used —
`check_order.rs` exists because SUM is order-independent and would pass a
compact that reversed, dropped, or duplicated rows.

**2. A compact interrupted mid-write leaves both intact.** Two tests from
different angles. `cancellation_leaves_the_original_cache_and_sidecar_intact`
cancels after the first column of a 5,000-row compact and asserts the cache is
**byte-identical** (`fs::read` comparison), the sidecar is byte-identical, no
scratch file survives, and both still open — the cache maps and reads its last
row, the sidecar loads against its fingerprint.
`a_crash_before_the_rename_changes_nothing` simulates the crash window
directly: it builds the scratch file and stops without committing, which is the
state a power cut leaves, then asserts the original is unchanged.

**3. Peak RAM for a multi-GB compact — measured (below).**

Others: idempotence (compacting an unedited cache reproduces it byte for byte),
formulas carried forward and reloadable against the new fingerprint, edits past
the end extending the sheet, error values surviving, the staleness fingerprint,
temp paths being siblings, and an assertion that 10× the rows does not move the
row-independent buffers at all. Three headless-harness tests drive the real app:
the enablement gate, the bake/retire/clear-undo path, and the post-compact
re-fingerprinting that lets a later save succeed.

## Measured peak RAM

Machine: Windows 11, this host. Release build. Fixture generated with
`gen-data`, 8 columns, then deleted.

| Source | Cache on disk | Rows | Compact time | Throughput | **Peak private bytes** | Peak working set |
|---|---|---|---|---|---|---|
| 3.22 GB | **3.60 GB** | 60,000,000 | 15.7 s | 337 MB/s | **7 MB** | 3,620 MB |
| 0.32 GB | 0.36 GB | 6,000,000 | 1.1 s | 461 MB/s | **5 MB** | 379 MB |

**A 10× larger file moved peak private memory by 2 MB.** That is the claim:
peak is one column stripe, independent of file size.

Two numbers are reported because on a memory-mapped workload they answer
different questions, and only one of them is the claim being made:

- **Private bytes** (7 MB) counts only pagefile-backed memory — heap, writer
  buffers, arena. This is what compact actually holds and cannot give back. It
  is flat.
- **Working set** (3,620 MB) also counts pages faulted in through the read-only
  mapping of the source. Streaming a 3.6 GB file touches all of them, so this
  tracks file size — but they are clean, file-backed pages the OS evicts the
  moment anything else wants the RAM. Quoting this as "peak RAM" would be
  misleading in the other direction, so both are printed.

The compactor's own accounting agrees: 3.1 MB of stripe writers + ~0 MB arena
(this fixture has 18 distinct strings) + ~0 MB edits = **3.2 MB**, unchanged
between the two runs.

### On the output being larger than the input (3.60 → 5.28 GB)

Not compactor overhead. The verifier deliberately writes **text** into columns
that previously held only numbers, and in the `.ferrix` format a column
containing any text needs a 4-byte-per-row string section it did not have
before. Four such columns × 60M rows × 4 bytes ≈ 1 GB, plus alignment. That is
the format working as designed. `compacting_twice_is_a_no_op_the_second_time`
pins the counterpart property: compacting with no edits reproduces the input
byte for byte.

`benchdata/` was removed after measuring.

## What I did NOT verify

- **No real power-cut test.** Crash safety is verified by construction and by
  the two interruption tests, not by pulling power mid-fsync. The ordering
  (temp → fsync → rename → delete) is the same one `export.rs` and `edits.rs`
  already use and which the 200M-row export was verified against.
- **No 10 GB run.** The largest measured compact is 3.60 GB. Peak private
  memory is flat across a 10× size range, and the loop is O(1) in rows by
  construction, but 10 GB itself was not run.
- **The UI was not driven through a real window.** Menu enablement, the
  compact lifecycle, undo clearing, and re-fingerprinting are covered by the
  headless harness (which calls the real `FerrixApp::frame`); the modal's
  pixels were not visually inspected.
- **Concurrency.** Compact is guarded against overlapping with load/export/
  another compact via `can_compact()`, but two Ferrix *processes* compacting
  the same file simultaneously is not defended against — no file locking was
  added.
- **Non-Windows.** Everything was built, tested, and measured on Windows only.
  The Unix branch of `mem_sample()` in the verifier is written but unexercised.
