# Working on Ferrix

Read this before starting an issue. Every rule here was learned by something
going wrong, and each one is cheap to follow and expensive to skip.

## Ground rules

**1. Work in your own clone.** Never edit a checkout you did not create.

```bash
cd <parent-of-ferrix> && git clone ferrix ferrix-<feature>
cd ferrix-<feature> && git checkout -b feat/<feature>
git config user.name "..."; git config user.email "..."   # repo-local, not --global
```

Two agents sharing one tree corrupt each other: contended builds, one agent's
`rm -rf benchdata` deleting another's fixtures mid-run, concurrent edits to one
file.

**2. All three gates pass before you are done.** CI enforces all three.

```bash
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Never pipe cargo through `tail`/`head` — the pipeline's exit code hides the real
one. Run bare, then check `$?`.

**3. Commit early and often.** As soon as something compiles with passing tests.
Do not hold hours of work uncommitted waiting for a perfect final state.

## The scale invariant

Ferrix targets 200M+ rows and 10GB+ files. Peak memory stays bounded by the
**viewport plus the edit overlay**, never by row count.

Concretely, for anything you add:

- Formatting, comments, merges, names and rules are stored **per column or per
  range**, never per cell. A rule over a 200M-row column is one small entry.
- Evaluation happens in the paint loop against **visible cells only**.
- Streaming work (import, export, compact) holds **one column stripe**, not the
  file. `convert.rs` does 10GB at 245 MB/s with a ~108MB peak that does not grow
  with file size — match that discipline.

If a design cannot hold this line, write a comment explaining why instead of
silently allocating per row.

## Testing

**Use the headless harness, not synthetic OS input.**
`crates/ferrix-ui/src/harness.rs` drives the real app through egui `RawInput`
and has 77 worked examples. Synthetic `SendInput`/computer-use is unreliable
against this egui app and has produced **four false bug reports**: clicks need a
preceding pointer-MOVE event, and Ctrl+key often loses the modifier. A failed
synthetic interaction is not evidence of a bug.

Modifiers go on `RawInput.modifiers`, **not** per-event — egui reads
`i.modifiers` from the aggregate.

Useful helpers already there: `select(a, b)`, `click_header(col)` (reads the
header's actual painted centre back from the app rather than hard-coding
pixels), `paint_text_count()` and `paint_shape_count()` (real shape counts from
frame output).

**Write assertions that can fail.** Ask of every assertion: *what would this
assert if the feature did nothing at all?*

A real example from this repo: a UI test asserted the status line was non-empty
and passed against a **completely dead gesture**, because the file-load message
already satisfied it. A test that passes for the wrong reason is worse than no
test — it certifies broken behaviour and stops anyone looking again.

Prefer asserting on state the feature changes: the resolved style of a cell, the
value at a coordinate, the count of painted shapes. Put context in the failure
message, not in the assertion.

**Pin the invariant, not just the happy path.** The existing suite pins things
like `Value <= 16 bytes`, cycle detection, and chunk-order preservation.

**Aggregates hide ordering bugs.** `SUM` is order-independent and passes even if
rows are reordered or dropped. `crates/ferrix-bench/src/check_order.rs` exists
because of exactly that trap. Verify the property the bug would violate —
per-row identity, exact counts, byte-identical round trips — not a total.

## Conventions

- **Formula rewriting edits formula TEXT, never the AST.** The parser discards
  the `$` markers the tokenizer records, so an AST round trip silently drops
  absolute-reference markers. Use `crates/ferrix-formula/src/remap.rs` and
  `refscan.rs`.
- **One row resolution path.** `RowResolver` in `crates/ferrix-ui/src/grid.rs`
  composes the table filter, the search filter and sort. Do not add a second
  mapping — two independent row mappings once painted **wrong records under
  correct row numbers**, and no single-feature test could see it.
- **Release builds use `panic = "unwind"` on purpose.** A worker-thread panic
  must not discard unsaved edits.
- **Report lossy round trips, never silently drop.** `rule_survives_xlsx` exists
  so the user learns in the editor that a rule has no Excel equivalent, rather
  than discovering it after opening the file in Excel.
- **Relocate every side-table when adding an indirection.** Routing base reads
  through a new display→data mapping while leaving a sparse side-table keyed by
  the old coordinate slides data out from under the user's edits, silently and
  plausibly.

## Cleanup

Generated fixtures go under `benchdata/` **in your own clone**, and are removed
when the measurement ends. Only clean your own tree — peers may be running.

## Reporting

If you are an agent reporting back to an orchestrator, keep a `REPORT.md` in
your clone root updated as you go: commit SHA, what landed, gate results, and
**what you did not verify**. A final message can be lost; a file in the clone
cannot. Be specific about the last one — "Excel was never launched, so this
proves well-formed OOXML, not that Excel accepts it" is the kind of honesty that
saves someone a day.
