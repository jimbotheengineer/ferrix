# Issue #42 — Workbook and sheet protection

Clone: `C:/Users/Error/projects/ferrix-protect`, branch `feat/protect`, forked
from `main` at `571c510`.

## Status

In progress. This file is updated as work lands.

## What landed so far

- `crates/ferrix-core/src/protect.rs` — the protection model.
  - `PasswordHash`: Excel's 16-bit ECMA-376-4 §18.2.29 hash, plus
    `matching_secret()` which manufactures a colliding string in constant
    time. That function is needed for the round trip (writers take a password,
    files carry only a hash) and doubles as the honesty demonstration.
  - `LockMap`: RANGE-keyed unlocked rectangles, `BTreeMap` by first row +
    tallest-height lookup window, same shape as `merge.rs`. Cells default to
    LOCKED, so the sparse set stored is the *unlocked* exceptions.
  - `SheetProtection` / `WorkbookProtection` / `Allowances` / `Denied` /
    `CellLockState`.
- `crates/ferrix-core/src/protect/tests.rs` — 22 tests.

## Gate results

Recorded at the end.

## Not verified

Recorded at the end.
