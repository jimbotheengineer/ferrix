# Ferrix roadmap #12 — cell comments / notes

Branch `feat/comments` in `C:/Users/Error/projects/ferrix-notes`.

## Gates (all three, clean)

- `cargo test --workspace` — **945 passed, 0 failed** (baseline was 909; +36)
- `cargo fmt --all --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean

## What was built

### `ferrix-core/src/comment.rs` (new) — the store
`CommentMap`: `BTreeMap<u32, Vec<(u32, Comment)>>` keyed by row, columns sorted
within a row so lookup binary-searches. Same shape as `MergeMap`, simpler
payload (a comment is one cell, not a rectangle, so no range query is needed).
`Comment { author, text }`; text clamped to `MAX_COMMENT_CHARS` (32,000) on a
char boundary at insert time.

Paint-path cost is the design constraint, and it is enforced two ways:
- `is_empty()` is a field read, so an uncommented sheet does **zero** map
  probes per frame.
- `row_comments(row)` answers for a whole row, so the grid hoists it out of the
  column loop: one probe per visible ROW, not per visible CELL.
- `probes()` / `reset_probes()` instrument this (atomic, `Relaxed`) so tests
  assert it rather than assume it.

`remap_columns(&HashMap<u32,u32>)` is a two-phase (vacate-all, then write-all)
relocation so a rotation cannot clobber itself. O(comments).

**Display vs underlying coordinates — the decision, reasoned in the module
docs:** comments are keyed by **display** position, like `EditOverlay`, and
relocated by the same code in `workbook.rs`. Keying by data coordinate would
desync the two stores: the overlay says "the user typed 42 here" at a display
coordinate, and a note saying "42 is provisional" must sit on the same cell.
Relocating one and not the other puts the note beside a different number —
plausible, wrong, invisible.

### `ferrix-io/src/comment_sidecar.rs` (new) — persistence
`.fxnotes` beside the base, separate from `.fxedits` and **not** guarded by the
base fingerprint (a note is a statement about a cell that survives the base
being regenerated — same reasoning as `.fxfmt`).

Layout reasoning is written into the module docs: comments are inherently
variable-length prose, so the seek-addressable fixed-width record layout
`format_sidecar.rs` uses is unavailable. An offset index was considered and
**rejected** — it buys random access to comment N, and every consumer loads all
comments at once. So it is a straight length-prefixed stream read front to back.
Record count is written up front and every string length-prefixed, so a
truncated file errors rather than silently yielding fewer comments. Records in
(row, col) order ⇒ byte-reproducible saves. An empty map *deletes* the sidecar.

### `ferrix-io/src/table_xlsx.rs` — xlsx round-trip (real, not a stub)
`write_comments` goes through `rust_xlsxwriter::Note`, which emits the **whole**
legacy note apparatus: `xl/comments1.xml` **plus** `xl/drawings/vmlDrawing1.vml`,
its rels, and the worksheet's `<legacyDrawing>`. The VML part is present — the
"silently emitting something Excel ignores" failure mode is avoided, and a test
asserts both parts exist and that the worksheet references the drawing.

`import_comments` opens the package with `zip` + `quick_xml`, reaches the
comments part through the **worksheet's relationships** (not by guessing
`comments{n}.xml` matches sheet n), concatenates all `<t>` runs, and resolves
`GeneralRef` entity events — without that last part quick-xml 0.41 reports
`&`, `<`, `>` as separate events and every such character in a note would be
silently deleted.

**Documented limitations** (in the `write_comments` doc comment): no reply
threading (v1 writes legacy notes, not `xl/threadedComments/*`; Excel offers to
convert, nothing is lost), and no box geometry/colour/visibility (Ferrix has no
model for them). Unattributed comments are written under a `"Ferrix"`
placeholder because `authorId` requires an author, and mapped back to empty on
import so a note the user never signed does not acquire an author.

**Not verified:** Excel itself was never launched — not available here. The
claim is "the parts Excel reads are present and structurally correct", proven by
unzipping the written package.

### UI
- `grid.rs`: `comments` field; row lookup hoisted out of the column loop; marker
  triangle in the **top-left** corner (deliberately the opposite corner and a
  different colour from the validation flag, so a cell that is both commented
  and invalid shows both facts); `hovered_comment`, `comment_markers`, and
  `context_click` on `GridResponse`; secondary-click routed through the *same*
  hit test as a left click.
- `theme.rs`: `comment_flag` in both palettes.
- `app.rs`: right-click cell menu (Insert/Edit Comment…, Delete Comment) as a
  positioned `Area` — the grid allocates no per-cell widget, so
  `Response::context_menu` is not available; comment editor window with
  author + multiline body, Ctrl+Enter to commit, Escape to cancel, Delete
  button; hover tooltip; empty text deletes rather than storing a blank note;
  keyboard handler yields to the editor while it is open.
- `workbook.rs`: `comments` field; `remap_columns` called in
  `remap_formulas_for_order` right beside the overlay relocation.

### Persistence wiring
Comments load on open (CSV/mmap: `.fxnotes`; xlsx: `xl/comments*.xml`, first
sheet) and are written by `save_comments()`, called **first and
unconditionally** inside `save_edits()` — the edits path returns early when the
overlay is empty, and a session that only added a note must not lose it.

## Tests added (36)

Core (14): round-trip, edit-not-duplicate, delete, undo/restore, row ordering,
iteration order, **200M-row sheet with 3 comments stores exactly 3 entries**,
**a cell with no comment costs zero probes / commented sheet probes once per
row**, non-colliding column rotation, clamping incl. multi-byte.

IO sidecar (9): round-trip incl. unicode/newlines/empty author, empty map
removes stale file, byte-reproducible saves, bad magic, bad version,
**truncation detected not silently shortened**, 3 comments over 200M rows write
< 512 bytes.

IO xlsx (4): package carries `xl/comments1.xml` **and** the VML drawing and the
worksheet's `legacyDrawing`, then re-imports identically; unattributed stays
unattributed; markup/newlines verbatim; no-comments workbook imports empty and
emits no comments part.

UI harness (9): marker painted on add; edit replaces; **delete removes the
MARKER** (asserted on real paint output); blank text deletes; **comment follows
its cell through a column reorder** and is not left behind; **paint-path cost**
(zero probes uncommented, ≤ rows when commented); 200M-shaped 3 entries;
**save + reopen restores both notes and their markers**; deleted comment does
not resurrect on reload.

### Negative controls run (tests proven non-vacuous)
- Commenting out `self.comments.remap_columns(&map)` → the reorder test fails
  with `left: None, right: Some("this 2 is provisional")`.
- Commenting out `comment_markers += 1` → 5 marker/persistence tests fail.
Both restored afterwards.

## Not done / notes
- xlsx comments are adopted for the **first sheet only**, matching the existing
  `merges` limitation (the workbook holds one comment map, for the active sheet).
- Comment relocation is not recorded in the undo entry: undoing a reorder does
  not restore `order` either, so re-deriving keeps comments and overlay
  consistent in every state reachable in the app. Reasoned in a code comment.
- No `benchdata/` was generated, so nothing to clean.
