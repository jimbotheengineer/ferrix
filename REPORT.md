# Named ranges (roadmap feature 4) — REPORT

Clone: `C:/Users/Error/projects/ferrix-names`, branch `feat/named-ranges`.

## Status: COMPLETE — all three gates green

```
cargo test --workspace                                    PASS  (878 tests, was 820)
cargo fmt --all --check                                   PASS
cargo clippy --workspace --all-targets -- -D warnings     PASS
```

Test counts by crate: core 344, io 135, formula 181 (+3 alloc), ui 203, misc 8+4.

## What was implemented

### `crates/ferrix-formula/src/names.rs` (new, ~780 lines with tests)
- `NameTable`, `DefinedName`, `NameScope::{Workbook, Sheet(String)}`.
- Sheet-scoped names shadow workbook-scoped ones **on their own sheet only**.
- `validate_name` enforces Excel's rules, including refusing anything that
  parses as a cell reference (`Tax1` = column TAX row 1) — a name spelled that
  way could never be reached, because the tokenizer resolves it as a ref first.
- `rename_in_formula` / `references_name` / `names_in` rewrite and scan formula
  **SOURCE TEXT** via the existing `refscan` machinery. Never an AST round trip
  — the parser discards `$` markers, so re-rendering would unpin every absolute
  reference in the workbook. There is a test for exactly this.
- `refers_to_range` renders a selection as `Sheet!$A$1:$B$9` (absolute, quoted
  sheet names where needed).
- Sheet rename/delete propagate through both scope and `refers_to`.

### `crates/ferrix-formula/src/parser.rs`
- New `parse_with_names(input, resolve)`. **Resolution happens in the parser**:
  a bare `Ident` not followed by `(` is looked up and REPLACED with the
  expression it stands for. `parse()` is now `parse_with_names(input, &|_| None)`,
  so behaviour without names is byte-identical to before.
- New `ParseError::UnknownName`, which the workbook renders as `#NAME?`.
- Function calls and cell references still win over the table (tested with a
  resolver that would claim everything).

### `crates/ferrix-formula/src/depgraph.rs`
- New `name_uses: HashMap<SheetCell, Vec<String>>` beside `precedents`, with
  `set_name_uses` / `name_uses_at` / `cells_using_name` / `rename_name_use`.
- Names vanish before edges are built (that is the scale invariant), so the
  graph records the words separately to find dependents for a rename/delete
  without an O(workbook) text rescan. Entries survive a failed parse, so a
  formula naming something undefined is revisited when it is later defined.

### `crates/ferrix-ui/src/workbook.rs`
- `Workbook::names: NameTable`; `parse_on(sheet, src)` is now the single parse
  path (commit, restore, eval, resync all route through it).
- `define_name`, `define_name_raw`, `retarget_name`, `rename_name`,
  `delete_name`, `name_for_selection`, `name_target`, `visible_names`,
  `parse_active`.
- `rename_sheet` / `delete_sheet` carry names along / drop local ones.

### `crates/ferrix-ui/src/app.rs`
- **Name Box** at the top-left of the formula bar, width-matched to
  `ROW_HEADER_WIDTH` so it sits above the row headers. Shows the selection's
  name or its A1 label; Enter navigates to an existing name, navigates to an
  A1 address, or defines a new name for the selection.
- **Name Manager** modal (`▾` button): list, Edit (rename + retarget), Go to,
  Delete. The Delete button shows the dependent count *before* the click.
- Formula-bar live preview now resolves names (`parse_active`).
- xlsx load populates `wb.names`; xlsx export writes them.

### `crates/ferrix-io/src/xlsx.rs`
- `import_defined_names(path) -> NameTable` and
  `export_workbook_with_names(path, sheets, names)`.
- Import opens `xl/workbook.xml` directly with quick-xml rather than using
  calamine's `defined_names()`, **because calamine drops `localSheetId`** — the
  only thing in the file distinguishing sheet scope from workbook scope.
  Without it every local name would be silently promoted on import.
- `_xlnm.*` built-ins (print areas etc.) are skipped.
- New `XlsxError::DefinedName`.

## Acceptance criteria — where each is tested

| criterion | test |
|---|---|
| `=SUM(Sales)` == `=SUM(Sheet1!B2:B1000)` | `workbook::tests::sum_of_a_named_range_equals_sum_of_the_explicit_range` |
| rename rewrites dependent formula TEXT | `workbook::tests::renaming_a_name_rewrites_the_source_text_of_every_dependent_formula`, `harness::tests::the_manager_renames_a_name_and_rewrites_dependent_formula_text` |
| ...and preserves `$` | `workbook::tests::a_rename_preserves_absolute_markers_in_the_rewritten_formula` |
| deleting a referenced name → `#NAME?` | `workbook::tests::deleting_a_referenced_name_turns_its_dependents_into_name_errors`, `harness::tests::the_manager_deletes_a_name_and_its_dependents_become_name_errors` |
| sheet- vs workbook-scoped same identifier | `workbook::tests::a_sheet_scoped_and_a_workbook_scoped_name_resolve_per_sheet` |
| xlsx round-trip preserves both scopes | `xlsx::tests::a_round_trip_preserves_both_workbook_and_sheet_scope` |
| scale invariant (no materialisation) | `workbook::tests::a_defined_name_never_materialises_the_range_it_spans` (1 rectangular precedent for a 1M-row name), `depgraph::tests::a_named_range_produces_the_same_edges_as_the_explicit_range`, `parser::tests::a_name_parses_to_the_very_same_tree_as_the_range_it_stands_for` |

Also asserted on the real OOXML, not just Ferrix-agrees-with-itself:
`xlsx::tests::exported_names_appear_as_real_defined_name_elements` unzips the
written file and checks for `<definedNames>`, `name="Rate"`, `name="Local"`,
and `localSheetId="1"`.

## Things I did NOT verify
- **Excel was never launched.** The claim is "the `<definedName>` OOXML Excel
  reads is present and structurally correct", verified by unzipping and
  inspecting the XML — same standard as the existing `table_xlsx` module states.
- No benchmark was run for the scale invariant. It is asserted structurally
  (the name resolves to the identical `Expr`, producing one rectangular
  precedent) rather than measured on a 200M-row file. `benchdata/` was never
  created, so nothing to clean up.
- Names are not persisted to the `.ferrix` edits sidecar (CSV path); they
  survive only through xlsx. The sidecar format was out of scope.
- Undo/redo does not cover name-table operations (defining/renaming/deleting a
  name is not an undoable step). The formula-text rewrites a rename performs are
  applied directly to the overlay.
- `name_box_sheet_scope` has a public setter and is honoured by
  `commit_name_box`, but no toggle widget is drawn for it yet; from the UI, the
  Name Box defines workbook-scoped names. Sheet-scoped names are creatable via
  the API, xlsx import, and are fully editable/deletable in the Name Manager.

## Note
Early on I accidentally applied one patch to the orchestrator's checkout at
`C:/Users/Error/projects/ferrix` instead of my clone. I reverted it immediately
with `git checkout --` and confirmed `git status` was clean before continuing.
No other writes touched that tree.
