# Named ranges (roadmap feature 4) — REPORT

Clone: `C:/Users/Error/projects/ferrix-names`, branch `feat/named-ranges`.

## Status: IN PROGRESS

## Done so far
- `crates/ferrix-formula/src/names.rs` — new. `NameTable`, `DefinedName`,
  `NameScope::{Workbook, Sheet(String)}`, validation, textual rename rewriting
  (`rename_in_formula`, `references_name`) built on the existing `refscan`
  machinery — never an AST round trip, so `$` markers survive.
- `crates/ferrix-formula/src/parser.rs` — `parse_with_names(input, resolve)`.
  Resolution happens IN THE PARSER: a bare `Ident` not followed by `(` is
  looked up in the name table and REPLACED with the expression it stands for,
  so `=SUM(Sales)` and `=SUM(Sheet1!B2:B1000)` are the same tree and the
  columnar fast path fires identically (scale invariant). Unknown word →
  new `ParseError::UnknownName`, which callers render as `#NAME?`.
  `parse()` is now `parse_with_names(input, &|_| None)`.

## Not yet done
- depgraph integration, ferrix-ui Name Box + Name Manager, xlsx read/write.

## Gates
Not yet run for the whole workspace.
