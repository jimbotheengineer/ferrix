# Issue #38 — Formula bar upgrades

Clone: `C:/Users/Error/projects/ferrix-fbar`, branch `feat/fbar`, forked from
`main` at `571c510`.

## Status

In progress.

## What has landed

- `crates/ferrix-formula/src/refedit.rs` — text-level reference editing built
  on `refscan`: `spans()` (ranges folded into one span), `span_at(caret)`,
  `cycle_at()` (F4's four states), `shift_span()` (drag a highlighted outline).
  No parse/render round trip anywhere, so `$` markers on untouched references
  survive byte-identically. 11 unit tests, including one that cycles a
  reference inside a formula full of other `$`-anchored references and asserts
  the whole string is unchanged after four presses.

## Gate results

Not yet run to completion on the full workspace.

## Not verified

- Nothing GUI-level yet.
