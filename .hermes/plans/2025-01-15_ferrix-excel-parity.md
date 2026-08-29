# Ferrix — Excel Parity Roadmap (Tier 2)

> **For Hermes:** This continues `2025-01-15_ferrix-roadmap.md` (features 1–14). Numbering picks up at 15. These are the features that turn Ferrix from "spreadsheet that opens the big files" into "the thing an Excel user can switch to without giving anything up."

**Goal:** Close the remaining Excel-parity gaps in the order they hurt a switching user.

**Non-goals:** VBA. COM automation. Real-time co-authoring UI (the CRDT groundwork in feature 14 is the substrate; presence/cursors ship later). Anything that only exists because Excel has legacy 1990s baggage (e.g. XLM macros).

**Same three rules as tier 1:** bounded memory at 200M rows, invariant tests, three gates (`test --workspace` / `fmt --check` / `clippy -D warnings`), harness-driven UI tests.

Already merged and NOT re-listed here: xlsx round-trip, structured tables with OOXML parts, validation model, format engine with column/range-scope storage, chart aggregation + panel, cross-sheet formulas, undo/redo with bulk-op rule.

---

## Category A — Formula engine parity

### Feature 15 — Dynamic arrays and spill

**Why:** The single biggest formula upgrade Excel made in the last decade. Once you have `UNIQUE`, `SORT`, `FILTER`, `SEQUENCE`, `RANDARRAY` returning ranges that "spill" into neighbouring cells, every sheet you write gets shorter. Also the substrate for LAMBDA and for query-shaped output from external connections (feature 22).

**Scope:**
- A formula whose result is a range spills into the cells to the right and below its host, painting them with a subtle marker and blocking edits inside the spill area.
- Overlap with existing content raises `#SPILL!` (matches Excel), with a hover tooltip showing which cell blocks it.
- New functions: `UNIQUE`, `SORT`, `SORTBY`, `FILTER`, `SEQUENCE(rows[,cols[,start[,step]]])`, `RANDARRAY`, `TOROW`, `TOCOL`, `WRAPROWS`, `WRAPCOLS`, `TAKE`, `DROP`, `CHOOSEROWS`, `CHOOSECOLS`, `HSTACK`, `VSTACK`.
- Implicit intersection operator `@` at parse time for back-compat with pre-spill formulas.
- Range operator `#` ("A1#" = the current spill of A1's formula).

**Files:**
- `crates/ferrix-formula/src/eval.rs` — Value gains a `Value::Spill(Array)` variant; range args accept it uniformly.
- `crates/ferrix-formula/src/parser.rs` — `@` prefix, `#` suffix.
- `crates/ferrix-formula/src/depgraph.rs` — spill cells are dependents of the host; their dep set is the host's plus the spill region.
- `crates/ferrix-ui/src/grid.rs` — paint the spill area under-layer + block-edit gesture.

**Verification:**
- `=SEQUENCE(10)` in A1 fills A1:A10; typing into A5 shows a block-edit affordance.
- `=SORT(FILTER(A:A, B:B>0))` returns the expected spilled column at 10M rows in < 100ms.
- xlsx round-trip: spilled formulas save with OOXML `<f t="array" ref="A1:A10">` and reload correctly.

---

### Feature 16 — LAMBDA and LET

**Why:** LAMBDA turns the sheet itself into the extensibility surface — a user can define reusable functions without touching Lua (feature 10) or recompiling. LET makes the formula bar readable for expressions that repeat the same subexpression three times.

**Scope:**
- `=LET(name1, expr1, name2, expr2, ..., body)` — evaluated left-to-right, later names may reference earlier.
- `=LAMBDA(x, y, x*y+1)(3, 4)` — inline application.
- Named LAMBDAs via the Name Manager (feature 4): a name whose value is a `=LAMBDA(...)` becomes callable as a function.
- Recursion allowed with a stack budget (existing `budget.rs`); exhaustion returns `#NUM!`.
- Helper trio: `MAP`, `REDUCE`, `SCAN`, `BYROW`, `BYCOL`, `MAKEARRAY`.

**Files:**
- `crates/ferrix-formula/src/parser.rs`, `eval.rs`
- `crates/ferrix-formula/src/names.rs` (from feature 4)

**Verification:**
- Recursive Fibonacci LAMBDA hits and reports the stack budget instead of hanging.
- Named LAMBDA + xlsx round-trip through `<definedName>` preserves the source text.

---

### Feature 17 — Structured references

**Why:** Tables ship. `=SUM(Sales[Amount])` does not, and every Excel table user relies on it.

**Scope:**
- `TableName[ColumnName]` — column reference
- `TableName[[#Headers],[Col]]` — header cell
- `TableName[[#Totals],[Col]]` — totals row (see feature 18)
- `TableName[@Col]` or `TableName[[#This Row],[Col]]` — the current row (context-dependent, only inside the table)
- `TableName[[Col1]:[Col5]]` — column range
- Renaming a table or column REWRITES formula text that used a structured reference (same rule as named ranges — safe because the name has no other meaning). Delete a table or column → `#REF!`.
- xlsx round-trip: OOXML uses these natively.

**Files:**
- `crates/ferrix-formula/src/parser.rs` (bracket-inside-bracket grammar)
- `crates/ferrix-formula/src/depgraph.rs` (table-shape changes invalidate references cheaply — table already knows its span)

**Verification:**
- `=SUM(Sales[Amount])` and `=SUM(Sales!C2:C1000)` return equal values.
- Adding a row to the table's bottom edge grows the reference automatically.
- Renaming column `Amount` → `Revenue` rewrites every formula that used `Sales[Amount]`.

---

### Feature 18 — Table totals row and calculated columns

**Why:** Two things you get free when you `Ctrl+T` in Excel and Ferrix currently doesn't.

**Scope:**
- Toggle: Table > Totals Row. Adds a row below the table; each column's cell is a dropdown (None, Sum, Average, Count, CountNumbers, Min, Max, StdDev, Var, or custom formula). Totals refresh on data change.
- Calculated column: type a formula into any body cell of a table column, and it auto-fills DOWN the entire column, rewriting refs per row. Editing any cell in that column offers "Overwrite all calculated cells in this column" (Excel's exact prompt) vs "This cell only".
- xlsx round-trip: OOXML `<tableStyleInfo>` totals + `<calculatedColumnFormula>` on both sides.

**Files:**
- `crates/ferrix-core/src/table.rs`
- `crates/ferrix-io/src/table_xlsx.rs`

---

### Feature 19 — Range operators: union, intersection, spaces

**Why:** Excel's range grammar is `A1:B5` (range), `A1:B5,D1:E5` (union), `A1:B5 B4:C10` (intersection — space is the operator). Ferrix has the first, needs the other two so power-user formulas port over.

**Scope:**
- Parser: `,` in a function call context stays argument-separator; `,` between two ranges inside parentheses is union. Space between two references is intersection.
- Empty intersection returns `#NULL!`.
- Implicit intersection interacts with feature 15's `@` operator; document precedence.

---

### Feature 20 — Formula bar upgrades

**Why:** The formula bar is where users spend actual time. Currently it is a single-line input.

**Scope:**
- Multiline expand (Alt+Enter inside a formula, drag-to-resize height).
- Function insert wizard: `Fx` button opens a searchable list of every formula; picking one drops a template with named argument placeholders.
- Argument tooltip: while typing `=VLOOKUP(`, a floating pill shows `VLOOKUP(lookup_value, table_array, col_index_num, [range_lookup])` with the current argument bolded.
- F4 cycles through absolute/mixed/relative on the reference under the caret (`A1` → `$A$1` → `A$1` → `$A1` → `A1`).
- Colour-matched range highlights: each range in the edited formula gets a colour, and the cells it covers get a matching outline in the grid. Excel-standard behaviour.
- `Ctrl+`` (backtick) toggles "show formulas" — every cell paints its source instead of its value.
- Formula error indicator: green triangle in the cell corner, click for a card explaining the error (division by zero, #NAME?, etc.).

**Files:**
- `crates/ferrix-ui/src/sheet_view.rs`
- `crates/ferrix-formula/src/eval.rs` (a "why is this an error" reporter)

---

### Feature 21 — Trace precedents / dependents

**Why:** Auditing a stranger's sheet is unusable without it.

**Scope:**
- Right-click cell → Trace Precedents / Trace Dependents / Remove Arrows.
- Draws arrow overlays from source cells to the selected cell, one hop at a time (Excel's exact model). Ctrl+click an arrow to jump.
- Uses the existing dep graph; no new storage.

---

## Category B — Data I/O and connections

### Feature 22 — External data connections (Power Query lite)

**Why:** The single biggest reason people leave Excel for Python is "I can't easily refresh my data." Ferrix has the columnar storage to do this well and can beat Excel's Power Query UI decisively by not shipping a second language.

**Scope (phased):**

**22a — Connection model.** A workbook can hold N named connections, each pointing at a source (CSV/TSV, JSON array-of-objects, HTTP GET returning either, SQLite/DuckDB file, ODBC DSN, Postgres URL). Refresh loads into a sheet designated as the connection's landing sheet.

**22b — Refresh.** Manual refresh from a button; auto-refresh on open (opt-in); refresh-on-interval (background). Streaming ingest same as CSV import — no full materialisation on the way in.

**22c — Query pane.** Small SQL editor over any structured connection (DuckDB is the natural in-process engine); the result lands as a spilled range (feature 15) in the target sheet.

**Risks:**
- DuckDB is a big dependency; feature-gate it behind `--features duckdb` so lean builds stay lean.
- Credentials: no plaintext in the workbook; store in the OS keychain (`keyring` crate).

---

### Feature 23 — Parquet and Arrow import/export

**Why:** Ferrix's storage is columnar. Parquet is columnar. There's a near-zero-copy path here that Excel does not have. This is Ferrix's flex.

**Scope:**
- Read: `.parquet` and `.arrow` open through the standard `File > Open`. `arrow2` or `parquet` crate.
- Write: `File > Export > Parquet` streams the current sheet out column by column.
- Type mapping: Number ↔ Float64/Int64, Text ↔ Utf8 (dictionary-encoded matches Ferrix's string-id path exactly), Bool ↔ Bool, Empty ↔ null, dates ↔ Timestamp.

---

### Feature 24 — Text-to-columns and Flash Fill

**Why:** Two "unglamorous but constant" tools.

**Scope:**
- Text-to-columns wizard: pick delimiter (comma/tab/semicolon/space/custom) or fixed-width (drag to place split lines), preview 100 rows, write to N columns.
- Flash Fill: user types a couple of examples in column B based on column A; Ctrl+E infers the pattern (regex + literal chunks) and fills the rest of the column. Excel's implementation is heuristic-plus-example-driven; a small library like `flashfill-rs` or an in-crate implementation works.
- Both operations are ONE undo step.

---

### Feature 25 — Import wizard with encoding and delimiter guessing

**Why:** CSV import currently assumes UTF-8, comma-delimited. Real-world CSVs are neither.

**Scope:**
- On open, if the file doesn't parse cleanly, show a wizard: encoding dropdown (auto-detected via `chardetng`), delimiter (auto-detected by counting candidates in first 100 lines), quote char, header row (yes/no/at row N), skip-N-rows-at-top.
- Preview grid updates live.
- "Save these settings for this filename" so reopening skips the wizard.

---

### Feature 26 — Clipboard interop with Excel

**Why:** Copy from Excel, paste into Ferrix, get what you expect. Currently CSV clipboard works; HTML clipboard (which Excel writes) does not.

**Scope:**
- Read the Windows/Linux HTML clipboard variant — Excel emits an HTML `<table>` with cell styling and formulas — and prefer it when present.
- Write both plain text (TSV) and HTML `<table>` on copy so pasting into Excel from Ferrix preserves alignment.
- Paste Special dialog: Values, Formulas, Formats, Column Widths, Transpose, Add/Subtract/Multiply/Divide (arithmetic paste), Skip Blanks.

---

## Category C — Cell rendering and editing

### Feature 27 — Number format engine (Excel format strings)

**Why:** The format engine exists but does not yet interpret Excel format strings like `#,##0.00;[Red](#,##0.00);"—";@`. Every table someone imports comes with these.

**Scope:**
- Parser for the four-section format string (`positive;negative;zero;text`), each section supporting placeholders `0`, `#`, `?`, `.`, `,`, `%`, `E+`, literal quoted text, `\c` escapes, colour tokens `[Red]`/`[Blue]`/`[Color 15]`, and conditional tokens `[>1000]`.
- Date/time tokens: `yyyy`, `yy`, `mmmm`, `mmm`, `mm`, `m`, `d`, `dd`, `ddd`, `dddd`, `h`, `hh`, `m` (context-sensitive), `mm`, `ss`, `AM/PM`, `[h]`/`[m]` (elapsed time).
- Locale-aware separator resolution (see feature 33 for locale).
- Format Cells dialog: Category (General, Number, Currency, Accounting, Date, Time, Percentage, Fraction, Scientific, Text, Custom) with the classic Excel preview panel.

**Files:**
- New `crates/ferrix-core/src/format/numfmt.rs`
- `crates/ferrix-io/src/xlsx.rs` — the parser/writer for `<numFmts>` becomes trivial once the engine exists.

**Verification:**
- Golden-file test: 200 canonical Excel format strings, each rendered against 10 sample values, byte-for-byte matches Excel's output.

---

### Feature 28 — Full cell styling

**Why:** Formatting shipped for font/size/bold/italic/underline (issue #18). The rest is table stakes.

**Scope:**
- Fill: solid colour, no gradient in v1.
- Borders: per-side (top/right/bottom/left/diagonal) style (none/thin/medium/thick/double/dotted/dashed) and colour. "Draw Borders" tool that lets a user click along grid lines.
- Alignment: horizontal (left/center/right/fill/justify), vertical (top/middle/bottom), indent 0–15, wrap text, shrink to fit, rotation −90..90 degrees.
- Text colour separate from fill colour.
- All stored on the range/column-scope engine that already exists — no per-cell inflation.

**Verification:**
- 10M rows with a column-scope fill: overlay stays under 1KB.
- xlsx round-trip for every combination.

---

### Feature 29 — Row/column sizing and visibility

**Why:** Autofit, hide, group. Missing from a basic-tools standpoint.

**Scope:**
- Drag a column border to resize (probably exists — verify); double-click to autofit to visible content (bounded pass: only measure the currently visible rows plus a sampled 1000 more; don't scan 200M).
- Right-click header → Hide / Unhide.
- Group: outline levels 1..8 with expand/collapse gutter on the left/top. Grouping is stored as ranges, not per-row.
- Row height 0 = hidden (Excel behaviour).

---

### Feature 30 — Merged cells

**Why:** Every Excel file coming in has these. Ferrix currently ignores merges on import.

**Scope:**
- Merge dropdown: Merge & Center, Merge Across (per-row within a range), Merge Cells, Unmerge.
- Merged cells store value in the top-left; other cells return `Empty` in formula land, are not editable directly.
- Sort/filter refuse a range containing merges (Excel behaviour: warns).
- xlsx round-trip via `<mergeCells>`.

---

### Feature 31 — Hyperlinks

**Why:** Cheap, expected, missing.

**Scope:**
- `=HYPERLINK(url, [display])` function.
- Ctrl+K insert-hyperlink dialog: link to URL / email / another cell / another sheet.
- Cell paints display text underlined in the theme's accent colour; Ctrl+click opens.
- xlsx round-trip.

---

### Feature 32 — Auto-complete in cells

**Why:** When you're typing a value into a column that already has values, Excel finishes the word from the column's existing distinct values. Very low friction, very missable.

**Scope:**
- On text edit in a cell, check the column's distinct-value set (already available in filter-dropdown code); if exactly one value starts with the typed prefix, show it as ghost text and Enter accepts.
- Skip on numeric columns.
- Cost: distinct set is per-column, cached, rebuilt on edit; capped at 10k entries (larger columns get no autocomplete — same as Excel).

---

## Category D — Locale, protection, accessibility

### Feature 33 — Locale and i18n

**Why:** European users type `,` as the decimal separator and `;` as the argument separator. Without locale, Ferrix is de facto US-only.

**Scope:**
- Detect from OS at first launch; overridable in Preferences.
- Locale affects: decimal separator, thousands separator, argument separator (formulas), date format defaults, currency symbol, day/month names.
- Formula parser: two modes (`, ,` and `; .`) selected once at parse; error message tells the user which they're in if `,` is ambiguous.
- xlsx round-trip: files carry their own locale, honoured on import regardless of user setting.

---

### Feature 34 — Sheet and workbook protection

**Why:** "Read-only for other users" is a workflow, not a security feature (Excel doesn't pretend otherwise), but many workbooks rely on it.

**Scope:**
- Sheet protection: lock cells (per-cell flag, defaults ON — matches Excel), the sheet-level "protected" flag actually enforces it. Password optional. Allowed actions per protected sheet: select/sort/filter/format/insert-rows/insert-cols/etc., all as individual toggles.
- Workbook protection: prevent adding/removing/renaming sheets.
- xlsx round-trip: `<sheetProtection>` and `<workbookProtection>`.
- Explicitly document that this is workflow protection, not encryption (see feature 35 for the encryption story).

---

### Feature 35 — Encrypted .xlsx read/write (optional)

**Why:** Enterprise workbooks are often password-encrypted (`aes-256-cbc`, ECMA-376). Refusing to open them is a hard stop for enterprise adoption.

**Scope:**
- Read: prompt for password, decrypt via the standard `msoffice-crypto` scheme. `office-crypto-rs` or similar.
- Write: opt-in; the "save with password" dialog.
- Feature-gated (`--features xlsx-crypto`) so builds without OpenSSL/rust-crypto stay lean.

---

### Feature 36 — Keyboard-only navigation and accessibility

**Why:** Excel is famously keyboard-driven. Every shortcut a power user has in their fingers should work.

**Scope:**
- Full Excel keyboard matrix:
  - `F2` edit cell, `F4` cycle absolute refs, `Ctrl+;` insert date, `Ctrl+Shift+;` insert time.
  - `Ctrl+D` fill down, `Ctrl+R` fill right, `Ctrl+Enter` fill selection.
  - `Ctrl+Shift+Arrow` extend selection to next non-empty; `Ctrl+Shift+End` to end of used range.
  - `Alt+=` insert SUM of adjacent range.
  - `Ctrl+Space` select column; `Shift+Space` select row.
  - `Ctrl+PageDown/PageUp` switch sheet.
- Screen-reader labels via egui accessibility (AccessKit) on the grid, formula bar, and all dialogs.
- Focus ring visible in all themes (dark and light).

---

### Feature 37 — Command palette ("Tell Me")

**Why:** Excel added it in 2016. Any modern app has it. Discoverability of features scales with palette not menus.

**Scope:**
- Ctrl+/ opens a palette listing every menu command, recent files, named ranges, sheet names, and functions. Fuzzy-match, Enter runs.
- Same widget powers a Function Insert overlay (feature 20).

---

## Category E — Data analysis tools

### Feature 38 — Remove duplicates, consolidate, subtotals

**Why:** Three menu items an analyst clicks weekly.

**Scope:**
- Remove Duplicates: choose columns to hash on; bulk delete; ONE undo step; reports N removed.
- Data > Subtotals: group by column X and insert subtotal rows every time X changes. Because a subtotal row is a derived artifact, store as a view transform (like sort), not as inserted data — sort/filter still work.
- Data > Consolidate: aggregate ranges from N sheets into a target range by row/column key.

---

### Feature 39 — Goal Seek

**Why:** The one what-if tool people actually use. Solver can wait; Goal Seek is 100 lines of secant method.

**Scope:**
- Data > Goal Seek: "Set cell A to value V by changing cell B". Iterate B, recompute the sheet, secant/bisection until |A−V| < ε or 100 iters.
- Refuses when A does not transitively depend on B (uses the dep graph — cheap check).

---

### Feature 40 — Slicers (for tables and pivots)

**Why:** Slicers are the visual filter. Once pivot (feature 9) ships, slicers are the discoverable way to drive them; also useful on plain tables.

**Scope:**
- Insert > Slicer for a table's column: a floating panel with buttons for each distinct value, click to filter, Ctrl+click for multi.
- Multiple slicers combine as AND across columns.
- Slicer bound to a pivot filters the pivot; bound to a table filters the underlying table.
- xlsx round-trip via `<slicer>` parts.

---

### Feature 41 — Sparklines

**Why:** Small, inline, high signal. Column of numbers next to a column of tiny line charts is the single densest useful visualisation in a spreadsheet.

**Scope:**
- Insert > Sparkline: for a target column, per-row sparkline drawn from a source row range (line, column, or win/loss).
- Rendered by the grid paint loop (no chart objects); scales trivially to any row count because painting cost is per-visible-row.
- xlsx round-trip via `<extLst><sparklineGroups>`.

---

### Feature 42 — Full chart objects (upgrade the existing chart panel)

**Why:** `chart_panel.rs` gets you a chart-of-the-selection view. Excel-style embedded chart OBJECTS (moveable, resizable, sitting on a sheet with a defined source range that refreshes on data change) are what people actually use.

**Scope:**
- Chart object stored on the sheet: source range spec, chart type, styling, position/size in pixels.
- Types: line, column, bar, area, scatter, pie, doughnut, combo (bar+line), stacked variants, 100%-stacked variants.
- Series editor: add/remove series, x-axis range, per-series name/colour/type.
- Trendlines: linear, exponential, moving average (window N); each is a scene primitive computed at draw time.
- Chart sheets: a whole sheet that IS a chart (matches Excel's Chart Sheet type).
- xlsx round-trip via `xl/charts/chartN.xml`.

---

## Category F — Files and formats

### Feature 43 — Native .ferrix workbook format

**Why:** `.ferrix` is currently a per-file columnar cache. There is no "one file that IS my workbook with edits, formats, tables, sheets, everything." Users have `.csv + .ferrix + .fxedits` triples on disk.

**Scope:**
- `.fxwb` (Ferrix workbook): a zip container mirroring xlsx's structure but with the native columnar layout as the payload (huge win for open time on multi-GB workbooks — Excel's own format is XML-in-zip and reads sequentially).
- Sheets inside a `.fxwb` may be mmapped exactly like standalone `.ferrix` files (store each sheet's columnar section 8-byte-aligned as today).
- File > Save As offers both `.fxwb` and `.xlsx`.

---

### Feature 44 — Print, PDF (upgrade), and HTML export

**Why:** Feature 13 (tier 1) landed basic PDF. Excel-parity print needs page setup, headers/footers, print area, page break preview.

**Scope:**
- Page Setup dialog: paper size, orientation, margins, scale/fit, print titles (repeat rows/cols), gridlines on/off, headers/footers with tokens `&P`/`&N`/`&D`/`&F`.
- Set Print Area / Clear Print Area.
- Page Break Preview mode: overlay the grid with page boundaries the user can drag.
- HTML export: single-file `.html` with a `<table>` and inline styles, one per sheet (or all in one file with tabs).

---

### Feature 45 — Recent files, templates, workbook thumbnails

**Why:** File > Recent is a permanent muscle memory. Templates give users a first thing to do.

**Scope:**
- Recent files list persisted; pinned files at the top.
- Templates gallery: budget, invoice, project plan, timesheet, personal finance. Each a `.fxwb` in the app resources folder.
- Thumbnail cached on save (small SVG of the first sheet's top-left) for the file picker.

---

## Category G — Extensibility, but not now

The following are **explicitly** deferred beyond the roadmap because they are big commitments with unclear ROI for the current user base. Listed so we don't forget:

- **Solver / Scenario Manager / What-if Data Tables** — the rest of the what-if suite beyond Goal Seek.
- **Full Power Pivot data model** — relationships between tables, DAX. Feature 22 covers the "get data in" half; a real semantic model is another year.
- **VBA / .xlsm support** — no.
- **Live collaboration UI (presence, cursors)** — the CRDT groundwork (feature 14) is prerequisite; the network layer is a separate project.
- **Add-in / plugin API** — after Lua (feature 10) settles, revisit.
- **Cloud sync** — out of scope for the desktop app.
- **Mobile/web version** — out of scope.

---

## Suggested execution order

Group in waves. Each wave is safe to run as a `parallel-agent-orchestration` fan-out.

**Wave A (immediate power-user unlocks):**
15 (spill) → 20 (formula bar) → 17 (structured refs) → 27 (number formats)

**Wave B (data ingestion story):**
25 (import wizard) → 26 (clipboard) → 23 (parquet/arrow) → 24 (TTC + Flash Fill)

**Wave C (make imports look right):**
28 (styling) → 30 (merged cells) → 29 (row/col sizing) → 31 (hyperlinks)

**Wave D (locale + accessibility):**
33 (locale) → 36 (keyboard/a11y) → 34 (protection) → 37 (command palette)

**Wave E (analysis tools):**
38 (dedupe/subtotals) → 39 (goal seek) → 42 (chart objects) → 40 (slicers) → 41 (sparklines)

**Wave F (formula deep end):**
16 (LAMBDA/LET) → 18 (table totals + calc cols) → 19 (range operators) → 21 (trace precedents) → 32 (autocomplete)

**Wave G (advanced I/O):**
22 (external connections) → 43 (native workbook format) → 35 (xlsx-crypto) → 44 (print/HTML) → 45 (templates)

At the end of wave A the average Excel user has a formula bar that feels right. At the end of wave B they can get their data in. At the end of wave C imported files look right. At the end of wave D the app is safe to hand to non-developers. At the end of wave E an analyst can do their job. At the end of wave F power users have parity. At the end of wave G Ferrix is a legitimate replacement.

Total features across both tier 1 (1–14) and tier 2 (15–45): **45 features, ~6–9 months of parallel-agent work depending on wave concurrency.**
