//! Workbook: edit application, recalculation, and undo/redo.
//!
//! This is where a keystroke becomes a committed, recalculated change. It owns
//! the overlay and the dependency graph and keeps them consistent, so the UI
//! only has to say "the user typed X into A1".

use ferrix_core::{
    CellInput, CellRef, EditOverlay, ErrorKind, Selection, SheetCell, SheetId, Value,
};
use ferrix_formula::depgraph::DepGraph;
use ferrix_formula::fill::FillKind;
use ferrix_formula::{eval_view, DefinedName, NameError, NameScope, ParseError};

use crate::grid::ScrollState;
use crate::sheet_view::{BaseData, SheetView};

/// One undoable action.
///
/// `changes` holds every cell the action touched, so a bulk operation (paste,
/// clearing a selected range) is a single undo step rather than one per cell.
/// A plain single-cell edit is just a one-element batch.
#[derive(Debug)]
pub struct UndoEntry {
    /// Which sheet the action happened on. Undo switches back to it, so
    /// rewinding always shows the user what actually changed.
    sheet: SheetId,
    /// Where to put the cursor when this entry is undone or redone.
    cell: CellRef,
    changes: Vec<CellChange>,
    /// Formula cells whose cached values changed as a side effect, so undo
    /// restores the whole visible state rather than leaving stale results.
    /// Addressed by [`SheetCell`] because a cross-sheet formula's cache can
    /// change as a side effect of an edit on a different tab.
    side_effects: Vec<(SheetCell, Value)>,
    /// True for bulk operations (paste, range clear, fill). Bulk entries are
    /// never coalesced into or out of: collapsing a paste into a neighbouring
    /// keystroke would make undo unpredictable.
    bulk: bool,
}

/// Default cap on the undo stack. A long session would otherwise grow it
/// without limit; 500 steps is far more than anyone reaches by hand and keeps
/// the memory cost bounded and predictable.
pub const DEFAULT_UNDO_LIMIT: usize = 500;

/// How long after an edit a further edit to the SAME cell folds into it.
///
/// Typing a value, immediately retyping it, and correcting it again is one
/// logical edit to a user; it should be one undo step. Anything slower than
/// this — or to a different cell — stays its own step.
pub const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(1000);

/// How many rows Replace All scans per window.
///
/// This is the knob that makes the scale invariant real. Peak memory during a
/// Replace All is one window's hits, so the window bounds it: at most
/// `REPLACE_WINDOW_ROWS x cols` `CellRef`s plus their text live at once,
/// regardless of whether the sheet has 4 rows or 200 million. Large enough
/// that the per-window arena pass is amortised to nothing; small enough that
/// a window of all-matching cells is kilobytes, not gigabytes.
pub const REPLACE_WINDOW_ROWS: usize = 65_536;

/// What a Paste Special actually did (issue #30).
///
/// Reported rather than inferred, so the status line can be specific and a
/// test can assert on the numbers instead of on a non-empty string.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PasteReport {
    /// Cells whose contents changed. Zero for a formats-only or widths-only
    /// paste, and for one whose every cell was skipped.
    pub cells_written: usize,
    /// Formatting RECTANGLES stored — not cells. A uniform format over a
    /// 100k-cell region is 1 here, which is the scale invariant made visible.
    pub format_rects: usize,
    /// Destination column widths to apply, as `(column, points)`. Non-empty
    /// only for [`ferrix_core::clipboard::PasteWhat::ColumnWidths`], because
    /// widths live in the app's sizing model rather than the workbook.
    pub col_widths: Vec<(u32, f32)>,
    pub transposed: bool,
    /// A caveat the user should see, if there is one.
    pub note: Option<String>,
}

/// `B2:D9` for a merged region, for a refusal message that names the region.
fn merge_label(r: ferrix_core::TableRange) -> String {
    format!(
        "{}{}:{}{}",
        ferrix_core::column_name(r.first_col),
        r.first_row + 1,
        ferrix_core::column_name(r.last_col),
        r.last_row + 1
    )
}

#[derive(Debug)]
struct CellChange {
    cell: CellRef,
    before: Option<CellInput>,
    after: Option<CellInput>,
}

/// Outcome of committing an edit, for status reporting.
#[derive(Debug, Default)]
pub struct CommitReport {
    pub recalculated: usize,
    pub circular: bool,
    pub parse_error: Option<String>,
    pub micros: u128,
    /// Set when protection refused the edit (issue #42). When this is
    /// `Some`, NOTHING was written: the cell holds exactly what it held
    /// before, and the caller is expected to show the reason.
    pub denied: Option<ferrix_core::Denied>,
}

// ============================================================================
// Goal Seek (issue #35)
// ============================================================================

/// Cap on secant iterations. ~100 per the acceptance criteria: enough for a
/// well-conditioned secant search to converge many times over, small enough
/// that a genuinely divergent target (see [`GoalSeekReport::converged`])
/// terminates in well under a second instead of spinning.
pub const GOAL_SEEK_MAX_ITERS: usize = 100;

/// Convergence tolerance: `|A - target| < GOAL_SEEK_EPSILON` counts as a hit.
pub const GOAL_SEEK_EPSILON: f64 = 1e-6;

/// Why a Goal Seek request was refused before it wrote anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalSeekError {
    /// `target` does not transitively depend on `changing` in the dependency
    /// graph, so no value written to `changing` could ever move `target`.
    /// Checked with [`ferrix_formula::depgraph::DepGraph::depends_on_at`]
    /// before a single recalculation runs.
    NotDependent,
    /// The changing cell holds a formula. Goal Seek would have to overwrite it
    /// with a bare number to search, which is silent data loss — and the value
    /// of a computed cell is not ours to choose. Refused before anything is
    /// written, like [`GoalSeekError::NotDependent`].
    ChangingCellIsFormula,
}

/// Outcome of a completed Goal Seek run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GoalSeekReport {
    /// True when the run stopped because `|target - target_value| < ε`,
    /// rather than because it hit the iteration cap or diverged.
    pub converged: bool,
    /// How many candidate values were actually committed to the changing
    /// cell (bounded by [`GOAL_SEEK_MAX_ITERS`], plus at most one closing
    /// write that restores the closest candidate found if the run ends on a
    /// worse one).
    pub iterations: usize,
    /// The value the caller asked for.
    pub target: f64,
    /// The changing cell's value at the end of the run — the closest
    /// approach found, whether or not it converged.
    pub final_b: f64,
    /// The target cell's value at the end of the run, if it evaluated to a
    /// number. `None` only when the target formula never produced a number
    /// for any candidate tried, which leaves the sheet at its original state
    /// (see [`Workbook::goal_seek`]).
    pub final_a: Option<f64>,
}

/// Where the user was last looking in a sheet.
///
/// Kept per sheet so switching tabs restores the position and selection the
/// user left behind, rather than dumping them back at A1 — a 200M-row sheet
/// makes losing your place genuinely expensive.
#[derive(Clone, Copy, Debug, Default)]
pub struct SheetViewState {
    pub scroll: ScrollState,
    pub selection: Selection,
}

/// A sheet's identity and view state. Its DATA is not here — see [`Workbook`].
#[derive(Debug)]
struct SheetMeta {
    id: SheetId,
    name: String,
    view: SheetViewState,
    /// Protection state for this sheet (issue #42).
    ///
    /// Lives beside the tab rather than beside the DATA because a parked
    /// sheet must keep its protection while its storage is swapped out — a
    /// sheet that quietly unprotected itself when the user clicked another
    /// tab would be worse than no protection at all.
    ///
    /// Bounded by the number of unlocked RECTANGLES, so a protected 200M-row
    /// sheet costs the same as a protected ten-row one.
    protection: ferrix_core::SheetProtection,
}

/// Why a sheet operation was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum SheetError {
    DuplicateName(String),
    EmptyName,
    LastSheet,
    NoSuchSheet,
    /// The workbook's structure is protected (issue #42).
    Protected(ferrix_core::Denied),
}

impl std::fmt::Display for SheetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SheetError::DuplicateName(n) => write!(f, "a sheet named {n:?} already exists"),
            SheetError::EmptyName => write!(f, "a sheet name cannot be blank"),
            SheetError::LastSheet => write!(f, "a workbook must keep at least one sheet"),
            SheetError::NoSuchSheet => write!(f, "no such sheet"),
            SheetError::Protected(d) => write!(f, "{d}"),
        }
    }
}

/// A workbook of one or more independently stored sheets.
///
/// ## Why the active sheet's storage lives in a field
///
/// `base` and `overlay` hold the ACTIVE sheet; every other sheet's storage is
/// parked in `parked`, keyed by id. Switching tabs swaps the two. That keeps
/// each sheet's storage genuinely independent — one can be a 12 GB mmap while
/// its neighbour is a small in-RAM scratch sheet — while leaving every read
/// path that already said `wb.base` / `wb.overlay` addressing the sheet the
/// user is looking at, unchanged and without a lookup.
///
/// The dependency graph, by contrast, is workbook-wide and keyed by
/// [`SheetCell`], because a formula chain does not respect tabs.
///
/// Adapts a display-position remap to the [`AxisMap`] the formula rewriter
/// wants. A column not in the map did not move.
struct ColumnMove {
    map: std::collections::HashMap<u32, u32>,
}

impl ferrix_formula::remap::AxisMap for ColumnMove {
    fn map(&self, old: u32) -> Option<u32> {
        Some(self.map.get(&old).copied().unwrap_or(old))
    }
}

pub struct Workbook {
    /// The ACTIVE sheet's immutable base. Never present in `parked`.
    ///
    /// Behind an `Arc` so a long export can take a handle to the base and
    /// stream it on a worker thread while the user keeps editing. The base is
    /// immutable by construction — every edit lands in the overlay — so
    /// sharing it needs no lock, and the clone is a refcount bump rather than
    /// a copy of a 12 GB mapping.
    pub base: std::sync::Arc<BaseData>,
    /// The ACTIVE sheet's edits. Never present in `parked`.
    pub overlay: EditOverlay,
    /// Workbook-wide dependency graph, spanning every sheet.
    pub graph: DepGraph,
    /// Defined names, workbook- and sheet-scoped.
    ///
    /// Beside the data, never inside it: a name is a handful of small strings
    /// however many rows it spans, and it is resolved to a plain range in the
    /// parser, so `=SUM(Sales)` over 200M rows costs exactly what the explicit
    /// range costs.
    pub names: ferrix_formula::NameTable,
    /// Tab order and per-sheet identity/view state. Never empty.
    sheets: Vec<SheetMeta>,
    /// Index into `sheets` of the sheet whose data is in `base`/`overlay`.
    active: usize,
    /// Merged regions for the ACTIVE sheet.
    ///
    /// Sparse rectangles beside the data, so merged headers cost nothing per
    /// row on a 200M-row sheet.
    pub merges: ferrix_core::merge::MergeMap,
    /// Cell comments for the ACTIVE sheet.
    ///
    /// Sparse, and keyed by DISPLAY position — the same space `overlay` is
    /// keyed in, and relocated by the same code. See `ferrix_core::comment`
    /// for why the two must agree: a note that stayed put while its cell moved
    /// would end up beside a different number, plausibly and invisibly wrong.
    pub comments: ferrix_core::CommentMap,
    /// Sheet-wide formatting for the ACTIVE sheet: manual colours and type
    /// styling.
    ///
    /// Beside the data, never inside it. Formatting a whole column or a
    /// 200M-row selection is one small entry, so styling costs nothing per
    /// row and appending rows inherits it for free.
    pub format: ferrix_core::SheetFormat,
    /// Display-order permutation for the ACTIVE sheet.
    ///
    /// Reordering a column must not move data: on a 200M-row sheet that would
    /// be gigabytes of copying for a gesture that should feel instant. The
    /// permutation lives here instead, and `view()` reads through it — so a
    /// reorder is O(cols) and the `.ferrix` file on disk is never rewritten.
    order: ferrix_core::SheetOrder,
    /// Storage for every INACTIVE sheet.
    parked: std::collections::HashMap<SheetId, (std::sync::Arc<BaseData>, EditOverlay)>,
    /// Monotonic id source. Never reused, so a deleted sheet's id can never
    /// be mistaken for a live one by a stale graph edge.
    next_id: u32,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    /// Maximum number of undo entries kept. When exceeded, the OLDEST entry is
    /// dropped — recent history is what users actually reach for.
    undo_limit: usize,
    /// The cell and wall-clock time of the last single-cell edit, used to
    /// decide whether the next edit folds into it. Cleared by anything that
    /// breaks the "same continuous edit" story: a bulk op, an undo, a redo, a
    /// save, a sheet switch, or an edit to a different cell.
    last_edit: Option<(SheetCell, std::time::Instant)>,
    /// Rows scanned per Replace All window. Defaults to
    /// [`REPLACE_WINDOW_ROWS`]; lowered by tests so a small fixture still
    /// crosses several window boundaries, which is the only way to catch a
    /// windowed walk that drops or double-counts a window.
    replace_window_rows: usize,
    /// Edits made since the last save. Drives the dirty indicator and the
    /// close prompt; without it a user can lose work by closing the window.
    dirty: bool,
    /// Workbook-structure protection (issue #42): the tab strip, not the
    /// cells. One small struct for the whole workbook.
    wb_protection: ferrix_core::WorkbookProtection,
    /// The most recent refusal by the protection guards, for the UI to say
    /// out loud. Cleared by every successful edit, so a stale message cannot
    /// be mistaken for a fresh refusal.
    ///
    /// A refusal that only returned `false` would satisfy the letter of
    /// "cannot edit a protected cell" while producing exactly the silent
    /// no-op the acceptance criteria call out.
    last_denial: Option<ferrix_core::Denied>,
}

impl Workbook {
    pub fn new(base: BaseData) -> Self {
        Self::with_name(base, "Sheet1")
    }

    /// A single-sheet workbook whose one sheet is called `name`.
    pub fn with_name(base: BaseData, name: &str) -> Self {
        Self {
            base: std::sync::Arc::new(base),
            overlay: EditOverlay::new(),
            graph: DepGraph::new(),
            names: ferrix_formula::NameTable::new(),
            sheets: vec![SheetMeta {
                // The first sheet is always MAIN, which is what makes a
                // single-sheet workbook's graph identical to the pre-sheets one.
                id: SheetId::MAIN,
                name: name.to_string(),
                view: SheetViewState::default(),
                protection: ferrix_core::SheetProtection::new(),
            }],
            active: 0,
            order: ferrix_core::SheetOrder::new(),
            format: ferrix_core::SheetFormat::new(),
            merges: ferrix_core::merge::MergeMap::new(),
            comments: ferrix_core::CommentMap::new(),
            parked: std::collections::HashMap::new(),
            next_id: 1,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_limit: DEFAULT_UNDO_LIMIT,
            last_edit: None,
            replace_window_rows: REPLACE_WINDOW_ROWS,
            dirty: false,
            wb_protection: ferrix_core::WorkbookProtection::new(),
            last_denial: None,
        }
    }

    // ---------------------------------------------------------------- sheets

    /// Tab order: every sheet's id and name, left to right.
    pub fn sheet_names(&self) -> Vec<(SheetId, &str)> {
        self.sheets
            .iter()
            .map(|s| (s.id, s.name.as_str()))
            .collect()
    }

    /// Used by tests and kept as workbook API; the UI reads the tab list
    /// directly rather than asking for a count.
    #[allow(dead_code)]
    pub fn sheet_count(&self) -> usize {
        self.sheets.len()
    }

    /// Used by tests and kept as workbook API; the UI reads the tab list
    /// directly rather than asking for a count.
    #[allow(dead_code)]
    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active_sheet(&self) -> SheetId {
        self.sheets[self.active].id
    }

    pub fn active_name(&self) -> &str {
        &self.sheets[self.active].name
    }

    pub fn sheet_name(&self, id: SheetId) -> Option<&str> {
        self.sheets
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.as_str())
    }

    /// Resolve a formula's sheet name to an id, case-insensitively.
    ///
    /// Excel treats sheet names as case-insensitive for lookup while
    /// preserving the case you typed; matching that means `sheet2!A1` finds
    /// `Sheet2` rather than silently becoming a `#REF!`.
    pub fn sheet_id_by_name(&self, name: &str) -> Option<SheetId> {
        self.sheets
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .map(|s| s.id)
    }

    fn index_of(&self, id: SheetId) -> Option<usize> {
        self.sheets.iter().position(|s| s.id == id)
    }

    /// Reject a name that is blank or collides with an existing sheet.
    /// `exclude` is the sheet being renamed, which may keep its own name.
    fn validate_name(&self, name: &str, exclude: Option<SheetId>) -> Result<String, SheetError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(SheetError::EmptyName);
        }
        let clash = self
            .sheets
            .iter()
            .any(|s| Some(s.id) != exclude && s.name.eq_ignore_ascii_case(trimmed));
        if clash {
            return Err(SheetError::DuplicateName(trimmed.to_string()));
        }
        Ok(trimmed.to_string())
    }

    // ------------------------------------------------------------ protection
    //
    // Issue #42. Read `ferrix_core::protect`'s module docs before touching
    // anything here: none of this is security, and the UI is required to say
    // so. What it does is stop an accidental keystroke landing in a formula
    // the sheet's author asked people not to touch.
    //
    // Enforcement lives at exactly two chokepoints — `guard_edit` for cells
    // and `check_structure` for the tab strip — so no edit path can grow its
    // own copy that drifts out of step.

    /// The ACTIVE sheet's protection state.
    pub fn protection(&self) -> &ferrix_core::SheetProtection {
        &self.sheets[self.active].protection
    }

    /// Mutable access to the ACTIVE sheet's protection.
    ///
    /// Marking a range unlocked is not itself an edit to any cell, so it is
    /// deliberately allowed while the sheet is protected: it is the author
    /// changing the rules, not a user editing through them. The UI is what
    /// gates who reaches this.
    pub fn protection_mut(&mut self) -> &mut ferrix_core::SheetProtection {
        self.dirty = true;
        let i = self.active;
        &mut self.sheets[i].protection
    }

    /// Another sheet's protection, by id. Used by export, which must write
    /// every sheet's `<sheetProtection>`, not only the visible one.
    ///
    /// Not yet called: the xlsx export path writes ONE sheet ("Sheet1"), so
    /// only the active sheet's protection currently reaches a file. These
    /// stay because they are the correct API for multi-sheet export, and are
    /// covered by `protect.rs`'s own tests — but the multi-sheet export
    /// itself remains open work under issue #42, and this comment is the
    /// marker for it. Do NOT read this as "multi-sheet protection export
    /// works".
    #[allow(dead_code)]
    pub fn protection_of(&self, id: SheetId) -> Option<&ferrix_core::SheetProtection> {
        self.sheets
            .iter()
            .find(|s| s.id == id)
            .map(|s| &s.protection)
    }

    #[allow(dead_code)]
    pub fn protection_of_mut(&mut self, id: SheetId) -> Option<&mut ferrix_core::SheetProtection> {
        self.dirty = true;
        self.sheets
            .iter_mut()
            .find(|s| s.id == id)
            .map(|s| &mut s.protection)
    }

    /// Adopt protection read from the FILE, without marking the workbook
    /// dirty (issue #42).
    ///
    /// Separate from `protection_mut` on purpose. That method marks dirty
    /// because a user changing the rules is a real change worth saving;
    /// loading a file that already carried those rules is not. Routing the
    /// load through `protection_mut` made every open — including a CSV with
    /// no protection at all — report unsaved changes, which broke five
    /// existing tests asserting that loading, searching and sorting never
    /// dirty the workbook.
    pub fn adopt_protection(
        &mut self,
        sheet: Option<ferrix_core::SheetProtection>,
        wb: ferrix_core::WorkbookProtection,
    ) {
        let was_dirty = self.dirty;
        if let Some(p) = sheet {
            let i = self.active;
            self.sheets[i].protection = p;
        }
        self.wb_protection = wb;
        self.dirty = was_dirty;
    }

    /// Workbook-structure protection.
    pub fn workbook_protection(&self) -> &ferrix_core::WorkbookProtection {
        &self.wb_protection
    }

    pub fn workbook_protection_mut(&mut self) -> &mut ferrix_core::WorkbookProtection {
        self.dirty = true;
        &mut self.wb_protection
    }

    /// The most recent protection refusal, if the last operation was refused.
    ///
    /// A SECOND channel for the same information, kept for callers that act
    /// on a refusal outside a commit. The UI does not use it: single edits
    /// read `CommitReport::denied` (see `FerrixApp::commit_edit`, which puts
    /// the reason in the status bar) and Replace All reports through
    /// `ReplaceReport::describe`. Both of those are exercised by tests; this
    /// pair is not, so it is marked rather than assumed live.
    #[allow(dead_code)]
    pub fn last_denial(&self) -> Option<ferrix_core::Denied> {
        self.last_denial
    }

    #[allow(dead_code)]
    pub fn clear_denial(&mut self) {
        self.last_denial = None;
    }

    /// THE cell-edit chokepoint. Every write to the overlay that originates
    /// from a user gesture passes through here.
    ///
    /// Returns `Err(reason)` when protection refuses the write, and records
    /// the reason so the UI can say it out loud rather than silently doing
    /// nothing — which is the acceptance criterion this exists for.
    fn guard_edit(&mut self, cell: CellRef) -> Result<(), ferrix_core::Denied> {
        match self.sheets[self.active].protection.deny_edit(cell) {
            Some(d) => {
                self.last_denial = Some(d);
                Err(d)
            }
            None => {
                self.last_denial = None;
                Ok(())
            }
        }
    }

    /// Refuse a bulk operation if ANY cell it would touch is protected.
    ///
    /// All-or-nothing on purpose: a paste that wrote the eleven unlocked cells
    /// of a twelve-cell block and dropped the twelfth would leave the user
    /// with a plausibly-shaped, silently wrong result. Better to refuse the
    /// whole thing and say which cell caused it.
    ///
    /// Bounded work: it stops at the first locked cell, and an unprotected
    /// sheet returns before iterating at all — so this cannot make a
    /// whole-column selection walk 200M cells on the common path.
    fn guard_range(&mut self, sel: Selection) -> Result<(), ferrix_core::Denied> {
        let prot = &self.sheets[self.active].protection;
        if !prot.is_enabled() {
            self.last_denial = None;
            return Ok(());
        }
        let (tl, br) = sel.bounds();
        // The whole rectangle is locked unless an unlocked range reaches into
        // it, so ask the range map first and only then walk cells.
        let touches_unlocked = prot.unlocked().ranges().any(|r| {
            r.first_row <= br.row
                && r.last_row >= tl.row
                && r.first_col <= br.col
                && r.last_col >= tl.col
        });
        let denied = if !touches_unlocked {
            prot.deny_edit(tl)
        } else {
            sel.iter().find_map(|c| prot.deny_edit(c))
        };
        match denied {
            Some(d) => {
                self.last_denial = Some(d);
                Err(d)
            }
            None => {
                self.last_denial = None;
                Ok(())
            }
        }
    }

    /// THE structure chokepoint: add / delete / rename / reorder.
    fn check_structure(&mut self, op: ferrix_core::StructureOp) -> Result<(), SheetError> {
        match self.wb_protection.deny(op) {
            Some(d) => {
                self.last_denial = Some(d);
                Err(SheetError::Protected(d))
            }
            None => {
                self.last_denial = None;
                Ok(())
            }
        }
    }

    /// A name no existing sheet uses, of the form `Sheet<N>`.
    pub fn unique_sheet_name(&self) -> String {
        let mut n = self.sheets.len() + 1;
        loop {
            let candidate = format!("Sheet{n}");
            if self.sheet_id_by_name(&candidate).is_none() {
                return candidate;
            }
            n += 1;
        }
    }

    /// Add a sheet with its own storage, immediately after the active tab.
    /// Does NOT switch to it; the caller decides.
    pub fn add_sheet(&mut self, name: &str, base: BaseData) -> Result<SheetId, SheetError> {
        self.check_structure(ferrix_core::StructureOp::AddSheet)?;
        let name = self.validate_name(name, None)?;
        let id = SheetId(self.next_id);
        self.next_id += 1;
        let at = self.active + 1;
        self.sheets.insert(
            at,
            SheetMeta {
                id,
                name,
                view: SheetViewState::default(),
                protection: ferrix_core::SheetProtection::new(),
            },
        );
        self.parked
            .insert(id, (std::sync::Arc::new(base), EditOverlay::new()));
        self.dirty = true;
        self.last_edit = None;
        Ok(id)
    }

    /// Rename a sheet. Refuses blank names and duplicates.
    ///
    /// Formula SOURCES that name the old sheet are deliberately NOT rewritten;
    /// `sheet_id_by_name` resolves through the current name list, so a formula
    /// pointing at a renamed sheet becomes a `#REF!` on next recalc rather
    /// than silently rebinding to whatever later takes that name. Rewriting
    /// sources is a bigger change than this issue asks for, and getting it
    /// half-right is worse than being explicit.
    pub fn rename_sheet(&mut self, id: SheetId, name: &str) -> Result<(), SheetError> {
        self.check_structure(ferrix_core::StructureOp::RenameSheet)?;
        let name = self.validate_name(name, Some(id))?;
        let idx = self.index_of(id).ok_or(SheetError::NoSuchSheet)?;
        let old = std::mem::replace(&mut self.sheets[idx].name, name.clone());
        // A defined name scoped to this sheet, or pointing into it, must
        // follow the rename or it would silently address a sheet that no
        // longer exists.
        self.names.rename_sheet(&old, &name);
        self.dirty = true;
        self.rebuild_graph_and_recalc();
        self.dirty = true;
        Ok(())
    }

    /// Delete a sheet and everything it stored. The last sheet cannot go.
    pub fn delete_sheet(&mut self, id: SheetId) -> Result<(), SheetError> {
        self.check_structure(ferrix_core::StructureOp::DeleteSheet)?;
        if self.sheets.len() == 1 {
            return Err(SheetError::LastSheet);
        }
        let idx = self.index_of(id).ok_or(SheetError::NoSuchSheet)?;
        // Captured before the tab goes, so sheet-scoped names can be dropped
        // by name afterwards.
        let deleted_name = Some(self.sheets[idx].name.clone());
        if idx == self.active {
            // Park the doomed sheet's storage so `base`/`overlay` can be
            // repointed at a survivor before we drop it.
            let neighbour = if idx + 1 < self.sheets.len() {
                self.sheets[idx + 1].id
            } else {
                self.sheets[idx - 1].id
            };
            self.activate(neighbour).expect("neighbour exists");
        }
        self.parked.remove(&id);
        let idx = self.index_of(id).expect("still present");
        self.sheets.remove(idx);
        // Reindex: `active` is a position, and removing a tab to its left
        // shifts it.
        if idx < self.active {
            self.active -= 1;
        }
        // Drop the deleted sheet's formulas, then re-resolve everything that
        // pointed AT it — those references are now #REF!.
        self.graph.remove_sheet(id);
        // A name scoped to the departed sheet goes with it; a workbook-scoped
        // name pointing INTO it is left alone, so it becomes a visible #REF!
        // rather than silently disappearing.
        if let Some(gone) = deleted_name.as_deref() {
            self.names.remove_sheet_scope(gone);
        }
        self.dirty = true;
        self.last_edit = None;
        self.rebuild_graph_and_recalc();
        Ok(())
    }

    /// Move a sheet to a new position in the tab strip.
    pub fn reorder_sheet(&mut self, id: SheetId, to: usize) -> Result<(), SheetError> {
        self.check_structure(ferrix_core::StructureOp::ReorderSheet)?;
        let from = self.index_of(id).ok_or(SheetError::NoSuchSheet)?;
        let to = to.min(self.sheets.len() - 1);
        if from == to {
            return Ok(());
        }
        let active_id = self.active_sheet();
        let meta = self.sheets.remove(from);
        self.sheets.insert(to, meta);
        // Order is presentation only; the active SHEET must not change just
        // because its index did.
        self.active = self.index_of(active_id).expect("active still present");
        self.dirty = true;
        Ok(())
    }

    /// Switch the active sheet, swapping storage and view state.
    pub fn activate(&mut self, id: SheetId) -> Result<(), SheetError> {
        let idx = self.index_of(id).ok_or(SheetError::NoSuchSheet)?;
        if idx == self.active {
            return Ok(());
        }
        let (base, overlay) = self.parked.remove(&id).ok_or(SheetError::NoSuchSheet)?;
        let old_id = self.sheets[self.active].id;
        let old_base = std::mem::replace(&mut self.base, base);
        let old_overlay = std::mem::replace(&mut self.overlay, overlay);
        self.parked.insert(old_id, (old_base, old_overlay));
        self.active = idx;
        // Leaving a sheet ends the coalescing run: coming back later must be
        // its own undo step.
        self.last_edit = None;
        Ok(())
    }

    /// Switch by tab position.
    pub fn activate_index(&mut self, index: usize) -> Result<(), SheetError> {
        let id = self.sheets.get(index).ok_or(SheetError::NoSuchSheet)?.id;
        self.activate(id)
    }

    /// The active sheet's saved scroll/selection.
    pub fn view_state(&self) -> SheetViewState {
        self.sheets[self.active].view
    }

    /// Record where the user is looking, so a tab switch can restore it.
    pub fn set_view_state(&mut self, state: SheetViewState) {
        self.sheets[self.active].view = state;
    }

    /// Used by tests and kept as workbook API; the UI reads the tab list
    /// directly rather than asking for a count.
    #[allow(dead_code)]
    pub fn view_state_of(&self, id: SheetId) -> Option<SheetViewState> {
        self.sheets.iter().find(|s| s.id == id).map(|s| s.view)
    }

    /// Read any sheet, active or parked, through the same composite view.
    pub fn sheet_view(&self, id: SheetId) -> Option<SheetView<'_>> {
        if id == self.sheets[self.active].id {
            return Some(SheetView::new(&self.base, &self.overlay));
        }
        self.parked.get(&id).map(|(b, o)| SheetView::new(b, o))
    }

    /// A resolver for the dependency graph, bound to the current name list.
    ///
    /// Returned by value (not borrowing `self`) so it can be handed to
    /// `&mut self` graph calls without fighting the borrow checker.
    fn name_resolver(&self) -> impl Fn(&str) -> Option<SheetId> + use<> {
        let names: Vec<(SheetId, String)> =
            self.sheets.iter().map(|s| (s.id, s.name.clone())).collect();
        move |n: &str| {
            names
                .iter()
                .find(|(_, name)| name.eq_ignore_ascii_case(n))
                .map(|(id, _)| *id)
        }
    }

    // ---------------------------------------------------------- defined names

    /// Parse a formula living on `home`, resolving defined names.
    ///
    /// Every parse in the workbook goes through here so that a name is
    /// resolved the same way whether the formula was just typed, restored from
    /// a sidecar, or imported from xlsx. The resolver is built by value rather
    /// than borrowing `self` so it can be handed to `&mut self` paths.
    fn parse_on(&self, home: SheetId, src: &str) -> Result<ferrix_formula::Expr, ParseError> {
        let sheet = self.sheet_name(home).map(str::to_string);
        let names = self.names.clone();
        ferrix_formula::parse_with_names(src, &move |ident| names.resolve(ident, sheet.as_deref()))
    }

    /// Parse a formula as the ACTIVE sheet would see it, defined names and all.
    ///
    /// The formula bar's live preview goes through here rather than the bare
    /// parser, so typing `=SUM(Sales)` previews the real value instead of
    /// reporting an error for a name that is perfectly well defined.
    pub fn parse_active(&self, src: &str) -> Result<ferrix_formula::Expr, ParseError> {
        self.parse_on(self.active_sheet(), src)
    }

    /// Every name visible from the active sheet, sheet-scoped first.
    pub fn visible_names(&self) -> Vec<&ferrix_formula::DefinedName> {
        self.names.visible_from(Some(self.active_name()))
    }

    /// The name, if any, whose target is exactly this selection on the active
    /// sheet. Drives what the Name Box shows.
    pub fn name_for_selection(&self, sel: Selection) -> Option<&str> {
        let (tl, br) = sel.bounds();
        let want = ferrix_formula::names::refers_to_range(self.active_name(), tl, br);
        self.visible_names()
            .into_iter()
            .find(|d| d.refers_to.eq_ignore_ascii_case(&want))
            .map(|d| d.name.as_str())
    }

    /// Where a visible name points, as a selection on the sheet it names.
    ///
    /// `None` when the name is unknown, or when it stands for something that
    /// is not a range on a sheet this workbook has (a constant, say) — there
    /// is nothing to navigate to in that case.
    pub fn name_target(&self, ident: &str) -> Option<(SheetId, Selection)> {
        let d = self.names.get(ident, Some(self.active_name()))?;
        let expr = d.target().ok()?;
        let home = self.active_sheet();
        match expr {
            ferrix_formula::Expr::Ref(c) => Some((home, Selection::single(c))),
            ferrix_formula::Expr::Range(a, b) => Some((home, Selection::new(a, b))),
            ferrix_formula::Expr::XRef(sheet, c) => {
                Some((self.sheet_id_by_name(&sheet)?, Selection::single(c)))
            }
            ferrix_formula::Expr::XRange(sheet, a, b) => {
                Some((self.sheet_id_by_name(&sheet)?, Selection::new(a, b)))
            }
            _ => None,
        }
    }

    /// Define a name for a selection on the active sheet.
    ///
    /// Formulas that already mention this word were `#NAME?` until now; they
    /// are re-evaluated so defining a name repairs them immediately.
    pub fn define_name(
        &mut self,
        ident: &str,
        scope: NameScope,
        sel: Selection,
    ) -> Result<(), NameError> {
        let (tl, br) = sel.bounds();
        let refers_to = ferrix_formula::names::refers_to_range(self.active_name(), tl, br);
        self.names
            .define(ferrix_formula::DefinedName::new(ident, scope, refers_to))?;
        self.dirty = true;
        self.refresh_name_dependents(ident);
        Ok(())
    }

    /// Define a name pointing at an explicit `refers_to` string.
    ///
    /// Used by xlsx import (which carries its own `refers_to` text verbatim)
    /// and by tests; the UI defines from a selection instead, so a release
    /// build of the binary sees this as unused.
    #[allow(dead_code)]
    pub fn define_name_raw(
        &mut self,
        ident: &str,
        scope: NameScope,
        refers_to: &str,
    ) -> Result<(), NameError> {
        self.names
            .insert(ferrix_formula::DefinedName::new(ident, scope, refers_to))?;
        self.dirty = true;
        self.refresh_name_dependents(ident);
        Ok(())
    }

    /// Point an existing name somewhere else.
    pub fn retarget_name(
        &mut self,
        ident: &str,
        scope: &NameScope,
        refers_to: &str,
    ) -> Result<(), NameError> {
        self.names.set_target(ident, scope, refers_to)?;
        self.dirty = true;
        self.refresh_name_dependents(ident);
        Ok(())
    }

    /// Rename a defined name, REWRITING the source text of every formula that
    /// uses it.
    ///
    /// This is safe in a way a sheet rename is not: a bare word that resolves
    /// to a name has no other possible meaning in a formula, so replacing it
    /// cannot change what anything else refers to. The rewrite is textual —
    /// see [`ferrix_formula::names::rename_in_formula`] — because an AST round
    /// trip would drop every `$` marker in the formula.
    ///
    /// Returns how many formulas were rewritten.
    pub fn rename_name(
        &mut self,
        ident: &str,
        scope: &NameScope,
        new: &str,
    ) -> Result<usize, NameError> {
        self.names.rename(ident, scope, new)?;
        // The canonical spelling as it now stands in the table, so rewritten
        // formulas read the way the Name Manager shows it.
        let canonical = self
            .names
            .get_scoped(new, scope)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| new.to_string());

        let cells = self.graph.cells_using_name(ident);
        let mut rewritten = 0usize;
        for at in cells {
            let Some(src) = self
                .overlay_of(at.sheet)
                .and_then(|o| o.get(at.cell))
                .and_then(|i| i.formula_src())
                .map(str::to_string)
            else {
                continue;
            };
            let new_src = ferrix_formula::names::rename_in_formula(&src, ident, &canonical);
            if new_src == src {
                continue;
            }
            if let Some(ov) = self.overlay_of_mut(at.sheet) {
                let cached = ov.value(at.cell).unwrap_or(Value::Empty);
                ov.set(
                    at.cell,
                    CellInput::Formula {
                        src: new_src,
                        cached,
                    },
                );
            }
            rewritten += 1;
        }
        self.graph.rename_name_use(ident, &canonical);
        self.dirty = true;
        self.rebuild_graph_and_recalc();
        self.dirty = true;
        Ok(rewritten)
    }

    /// Delete a name. Formulas that used it keep their text and become
    /// `#NAME?` — the parser no longer has an entry for the word, which is
    /// precisely what `#NAME?` means. Rewriting them to `#REF!` would throw
    /// away text the user can recover simply by redefining the name.
    pub fn delete_name(&mut self, ident: &str, scope: &NameScope) -> Option<DefinedName> {
        let removed = self.names.remove(ident, scope)?;
        self.dirty = true;
        self.refresh_name_dependents(ident);
        Some(removed)
    }

    /// Re-evaluate every formula whose text mentions `ident`, plus everything
    /// downstream of them. Called after any change to the name table.
    fn refresh_name_dependents(&mut self, ident: &str) {
        if self.graph.cells_using_name(ident).is_empty() {
            return;
        }
        self.rebuild_graph_and_recalc();
        // `rebuild_graph_and_recalc` clears the dirty flag (it is used for
        // restore, where the file already matches). A name edit IS a change.
        self.dirty = true;
    }

    // --------------------------------------------------------------- editing

    /// Shrink the Replace All scan window. Test seam; see the field's docs.
    pub fn set_replace_window_rows(&mut self, rows: usize) {
        self.replace_window_rows = rows.max(1);
    }

    /// Override the undo depth cap. Mainly for tests and future configuration;
    /// a limit of 0 disables undo entirely.
    #[allow(dead_code)]
    pub fn set_undo_limit(&mut self, limit: usize) {
        self.undo_limit = limit;
        self.enforce_undo_limit();
    }

    #[inline]
    #[allow(dead_code)]
    pub fn undo_limit(&self) -> usize {
        self.undo_limit
    }

    /// End the current coalescing run without touching history.
    ///
    /// The next edit — even to the same cell, even immediately — starts a new
    /// undo entry. The UI calls this when the user leaves the cell, so moving
    /// away and coming back is two undo steps rather than one.
    #[inline]
    pub fn end_edit_run(&mut self) {
        self.last_edit = None;
    }

    /// Drop history from the bottom until the stack fits the cap.
    fn enforce_undo_limit(&mut self) {
        if self.undo.len() > self.undo_limit {
            let excess = self.undo.len() - self.undo_limit;
            self.undo.drain(0..excess);
        }
    }

    /// Discard undo and redo history, returning how many undo steps were lost.
    ///
    /// Called on save: history is deliberately NOT persisted (see README), and
    /// undoing past a save would leave the file on disk and the screen telling
    /// different stories. The caller is expected to say so in the UI rather
    /// than dropping it silently.
    pub fn clear_history(&mut self) -> usize {
        let lost = self.undo.len();
        self.undo.clear();
        self.redo.clear();
        self.last_edit = None;
        lost
    }

    /// Adopt edits loaded from a sidecar. Marks the workbook clean, since what
    /// is in memory now matches what is on disk.
    pub fn with_overlay(mut self, overlay: EditOverlay) -> Self {
        self.overlay = overlay;
        self.dirty = false;
        self
    }

    /// Swap the ACTIVE sheet's immutable base for a different one.
    ///
    /// Exists for Compact, which rewrites the `.ferrix` file underneath a live
    /// mapping. On Windows a mapped file cannot be renamed over, so the UI
    /// must let go of the old mapping before the commit and adopt the new one
    /// after — this is that handover. Everything else (overlay, formats,
    /// merges, order) is by construction independent of which base is under
    /// it, so nothing else needs to move.
    pub fn replace_base(&mut self, base: BaseData) {
        self.base = std::sync::Arc::new(base);
    }

    /// Replace the ACTIVE sheet's overlay in place.
    ///
    /// Used when loading a multi-sheet workbook: each sheet is added, made
    /// active, and handed the formulas that came with it. Does not touch the
    /// dirty flag — the caller decides whether loading counts as a change.
    pub fn adopt_overlay(&mut self, overlay: EditOverlay) {
        self.overlay = overlay;
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Flag the workbook clean WITHOUT touching undo history.
    ///
    /// Superseded by `save_committed` on the normal save path, which also
    /// clears history per the documented behaviour. Kept for callers that
    /// need to mark clean without discarding a timeline.
    #[inline]
    #[allow(dead_code)]
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// Mark the workbook saved AND drop undo/redo history, returning how many
    /// undo steps were discarded so the UI can report it.
    ///
    /// This is the documented save behaviour (README, "Editing"): history is
    /// cleared on save rather than persisted. It is the honest option — the
    /// sidecar stores the overlay, not a timeline, so undo cannot meaningfully
    /// cross a save — and it is surfaced in the status bar rather than dropped
    /// in silence.
    pub fn save_committed(&mut self) -> usize {
        self.dirty = false;
        self.clear_history()
    }

    /// Counterpart to `mark_saved`; kept so callers that mutate state outside
    /// the normal edit path can flag the workbook. Unused today.
    #[inline]
    #[allow(dead_code)]
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Rebuild the dependency graph from every formula in the overlay and
    /// recompute their values.
    ///
    /// Used after restoring saved edits: the sidecar stores formula *source*
    /// plus a cached result, but that cache was computed against the base as
    /// it stood at save time. Re-evaluating guarantees what the user sees
    /// matches the data actually in front of them.
    pub fn rebuild_graph_and_recalc(&mut self) {
        self.graph = DepGraph::new();
        let resolve = self.name_resolver();
        // Every sheet's formulas, not just the active one — a parked sheet's
        // formula is still a live node in the workbook graph.
        let ids: Vec<SheetId> = self.sheets.iter().map(|s| s.id).collect();
        let mut formulas: Vec<(SheetCell, String)> = Vec::new();
        for id in ids {
            let Some(ov) = self.overlay_of(id) else {
                continue;
            };
            for (cell, src) in ov.formula_cells() {
                formulas.push((SheetCell::new(id, cell), src.to_string()));
            }
        }
        for (at, src) in &formulas {
            if let Ok(expr) = self.parse_on(at.sheet, src) {
                self.graph.set_formula_at(*at, &expr, &resolve);
            }
            // Recorded from the TEXT regardless of whether the parse succeeded:
            // a formula reading `=SUM(Sales)` while `Sales` is undefined is
            // exactly the one that must be revisited when it is defined.
            self.graph.set_name_uses(*at, src);
        }
        // Evaluate in dependency order so a formula referencing another
        // formula sees an up-to-date value rather than a stale cache.
        // Cycles come back as Err; those cells get #CIRC! rather than a stale
        // cached value silently surviving from the saved file.
        let (order, circular) = match self.graph.full_order_all() {
            Ok(o) => (o, Vec::new()),
            Err(stuck) => (Vec::new(), stuck),
        };
        for at in circular {
            if let Some(ov) = self.overlay_of_mut(at.sheet) {
                ov.update_cached(at.cell, Value::Error(ErrorKind::Circular));
            }
        }
        for at in order {
            self.eval_one_at(at);
        }
        // A formula whose sheet reference no longer resolves is not in the
        // graph at all, so the topological pass above never touches it. Sweep
        // every remaining formula so a dangling `Sheet2!A1` becomes #REF!
        // instead of keeping the value it had when Sheet2 existed.
        for (at, _) in &formulas {
            if self.graph.precedents_at(*at).is_none() {
                self.eval_one_at(*at);
            }
        }
        // Restoring is not an edit; the file on disk already matches.
        self.dirty = false;
    }

    pub fn view(&self) -> SheetView<'_> {
        SheetView::with_order(&self.base, &self.overlay, &self.order)
    }

    /// Move `count` columns starting at display position `from` so they land
    /// at display position `to`.
    ///
    /// This permutes the display order; it does not touch a single cell of
    /// data. On a 200M-row sheet that is the difference between instant and
    /// several seconds of copying.
    ///
    /// Formulas are rewritten so they keep referring to the same DATA. A
    /// formula reading `=SUM(B1:B10)` must still sum the same values after B
    /// is dragged elsewhere, or a reorder would silently change every result
    /// on the sheet.
    pub fn move_columns(&mut self, from: u64, count: u64, to: u64) -> Result<(), String> {
        let cols = self.view().col_count().max(1) as u64;
        let before = self.order.clone();

        self.order
            .cols_mut(cols)
            .move_span(from, count, to)
            .map_err(|e| format!("{e:?}"))?;

        // Rewriting formula TEXT rather than the AST, for the same reason fill
        // does: the parser discards the `$` markers the tokenizer records, so
        // an AST rewrite would silently unpin every absolute reference.
        let remapped = self.remap_formulas_for_order(&before);

        self.push_undo(UndoEntry {
            sheet: self.active_sheet(),
            cell: CellRef::new(0, from as u32),
            changes: remapped,
            side_effects: Vec::new(),
            bulk: true,
        });
        self.dirty = true;
        self.recalc_all();
        Ok(())
    }

    /// Rewrite every formula so its references survive an order change.
    ///
    /// Returns the edits made, so the whole reorder collapses into one undo
    /// step rather than one per formula.
    fn remap_formulas_for_order(&mut self, before: &ferrix_core::SheetOrder) -> Vec<CellChange> {
        use ferrix_formula::remap::remap_columns;

        // Display position a data column occupied before, and occupies now.
        // The map a formula needs is old-display -> new-display.
        let cols = self.view().col_count().max(1) as u64;
        let mut map = std::collections::HashMap::new();
        for old_display in 0..cols {
            let data = before
                .cols
                .as_ref()
                .and_then(|a| a.data_of(old_display))
                .unwrap_or(old_display as u32);
            let new_display = self
                .order
                .cols
                .as_ref()
                .and_then(|a| a.display_of(data))
                .unwrap_or(data as u64);
            if old_display != new_display {
                map.insert(old_display as u32, new_display as u32);
            }
        }
        if map.is_empty() {
            return Vec::new();
        }

        // Comments ride along with the overlay, for the same reason and by
        // the same map. THE PITFALL: routing base reads through a new display
        // indirection while leaving a sparse side-table keyed by the
        // pre-indirection coordinate slides data out from under the user
        // silently — the note would stay on screen column B while the number
        // it describes moved to D. `remap_columns` is a single two-phase pass
        // so a rotation cannot clobber itself, and costs O(comments).
        //
        // Deliberately NOT recorded in the undo entry. Undoing a reorder does
        // not currently restore `order` either — only the overlay cells — so
        // relocating comments back would leave them keyed against a
        // permutation still in effect, i.e. desynced from the very cells they
        // annotate. Comments track whatever order is live, which keeps the two
        // stores consistent with each other in every state the app can reach.
        self.comments.remap_columns(&map);

        let mover = ColumnMove { map: map.clone() };
        let mut changes = Vec::new();

        // Overlay cells are keyed by DISPLAY position, while base data is
        // reached through the order. So a reorder that only permutes the order
        // moves the base values and strands every edit on the wrong column:
        // the user's typed value and their formula stay put while the data
        // slides out from under them.
        //
        // Relocating the overlay keeps the two in step. It costs O(edits),
        // never O(rows) — the whole reason edits live in a sparse overlay.
        let existing: Vec<(CellRef, CellInput)> = self
            .overlay
            .edited_cells()
            .map(|(c, i)| (*c, i.clone()))
            .collect();

        for (cell, input) in &existing {
            let Some(&new_col) = map.get(&cell.col) else {
                continue;
            };
            // Clear the old position, then write the relocated one below.
            if let Some(prev) = self.overlay.clear(*cell) {
                changes.push(CellChange {
                    cell: *cell,
                    before: Some(prev),
                    after: None,
                });
            }
            let _ = (new_col, input);
        }

        for (cell, input) in existing {
            let Some(&new_col) = map.get(&cell.col) else {
                continue;
            };
            let dest = CellRef::new(cell.row, new_col);
            // A formula's TEXT is rewritten as well as its position, so it
            // keeps reading the same data from its new home.
            let moved = match &input {
                CellInput::Formula { src, .. } => CellInput::Formula {
                    src: remap_columns(src, &mover),
                    cached: Value::Empty,
                },
                other => other.clone(),
            };
            let prev = self.overlay.set(dest, moved.clone());
            changes.push(CellChange {
                cell: dest,
                before: prev,
                after: Some(moved),
            });
        }

        changes
    }

    /// How many undo entries are stacked. Exposed so tests can assert that a
    /// bulk operation is one step rather than one per cell.
    #[allow(dead_code)] // used by tests only; kept as workbook API
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn edit_count(&self) -> usize {
        self.overlay.len()
    }

    /// Parse raw user input into a `CellInput`.
    ///
    /// Order matters: a leading `=` means formula; otherwise we try number,
    /// then bool, then fall back to text. Empty input clears the cell.
    fn classify(&mut self, raw: &str) -> Option<CellInput> {
        let t = raw.trim();
        if t.is_empty() {
            return None;
        }
        if t.starts_with('=') {
            return Some(CellInput::Formula {
                src: t.to_string(),
                cached: Value::Empty, // filled in by recalc
            });
        }
        if let Ok(n) = t.parse::<f64>() {
            return Some(CellInput::Literal(Value::Number(n)));
        }
        match t.to_ascii_uppercase().as_str() {
            "TRUE" => return Some(CellInput::Literal(Value::Bool(true))),
            "FALSE" => return Some(CellInput::Literal(Value::Bool(false))),
            _ => {}
        }
        let id = self.overlay.intern(t);
        Some(CellInput::Literal(Value::Text(id)))
    }

    /// Commit what the user typed into `cell` on the ACTIVE sheet, then
    /// recalculate dependents — including any on other sheets.
    ///
    /// Protection (issue #42) is enforced HERE, at the single chokepoint every
    /// typed edit passes through, rather than at the several call sites that
    /// reach it. A refused edit sets [`CommitReport::denied`] and writes
    /// nothing at all — the overlay, the graph and the dirty flag are left
    /// exactly as they were, so "refused" is observable as the value not
    /// changing, not merely as a message appearing.
    pub fn commit_edit(&mut self, cell: CellRef, raw: &str) -> CommitReport {
        let start = std::time::Instant::now();
        let mut report = CommitReport::default();
        if let Err(denied) = self.guard_edit(cell) {
            report.denied = Some(denied);
            report.micros = start.elapsed().as_micros();
            return report;
        }
        self.dirty = true;
        let sheet = self.active_sheet();
        let at = SheetCell::new(sheet, cell);

        let new_input = self.classify(raw);
        let before = self.overlay.get(cell).cloned();

        // Apply to the overlay first so evaluation sees the new value.
        match &new_input {
            Some(input) => {
                self.overlay.set(cell, input.clone());
            }
            None => {
                self.overlay.clear(cell);
            }
        }

        // Update the dependency graph for this cell.
        match &new_input {
            Some(CellInput::Formula { src, .. }) => {
                match self.parse_on(sheet, src) {
                    Ok(expr) => {
                        let resolve = self.name_resolver();
                        self.graph.set_formula_at(at, &expr, &resolve);
                        // Sheet-aware: a cycle that leaves this sheet and comes
                        // back is caught here just like a local one.
                        if self.graph.is_circular_at(at) {
                            report.circular = true;
                            self.overlay.set(
                                cell,
                                CellInput::Formula {
                                    src: src.clone(),
                                    cached: Value::Error(ErrorKind::Circular),
                                },
                            );
                        }
                    }
                    Err(e) => {
                        report.parse_error = Some(e.to_string());
                        self.graph.remove_at(at);
                        self.overlay.set(
                            cell,
                            CellInput::Formula {
                                src: src.clone(),
                                cached: Value::Error(ErrorKind::Name),
                            },
                        );
                    }
                }
                // Record the names the TEXT mentions, AFTER the match: the
                // error arm calls `remove_at`, which clears them. A formula
                // reading `=SUM(Sales)` while `Sales` is undefined is exactly
                // the one that must be revisited when it is defined, so its
                // name use has to survive the failed parse.
                self.graph.set_name_uses(at, src);
            }
            _ => {
                // No longer a formula (or cleared): drop its edges.
                self.graph.remove_at(at);
            }
        }

        // Evaluate this cell if it is a healthy formula.
        if !report.circular && report.parse_error.is_none() {
            self.eval_one_at(at);
        }

        // Recalculate everything downstream, in dependency order. This spans
        // sheets: a Sheet2 formula reading this cell is in the same order.
        let mut side_effects = Vec::new();
        match self.graph.recalc_order_at(at) {
            Ok(order) => {
                for dep in order {
                    let prev = self
                        .overlay_of(dep.sheet)
                        .and_then(|o| o.value(dep.cell))
                        .unwrap_or(Value::Empty);
                    self.eval_one_at(dep);
                    let now = self
                        .overlay_of(dep.sheet)
                        .and_then(|o| o.value(dep.cell))
                        .unwrap_or(Value::Empty);
                    if prev != now {
                        side_effects.push((dep, prev));
                    }
                    report.recalculated += 1;
                }
            }
            Err(cycle) => {
                report.circular = true;
                for c in cycle {
                    if let Some(ov) = self.overlay_of_mut(c.sheet) {
                        ov.update_cached(c.cell, Value::Error(ErrorKind::Circular));
                    }
                }
            }
        }

        let after = self.overlay.get(cell).cloned();
        let now = std::time::Instant::now();

        // Coalesce: if the previous undo entry is a single-cell edit to THIS
        // same cell ON THIS SHEET, made within COALESCE_WINDOW, fold this edit
        // into it so one logical edit is one undo step. Keep the older
        // `before` (undo must rewind to the state before the burst started)
        // and adopt the new `after`.
        let coalesce = match (self.last_edit, self.undo.last()) {
            (Some((last_at, then)), Some(top)) => {
                last_at == at
                    && !top.bulk
                    && top.sheet == sheet
                    && top.changes.len() == 1
                    && top.changes[0].cell == cell
                    && now.duration_since(then) < COALESCE_WINDOW
            }
            _ => false,
        };

        if coalesce {
            let top = self.undo.last_mut().expect("checked above");
            top.changes[0].after = after;
            // Merge side effects, keeping the FIRST recorded prior value for
            // each dependent — that is the one that predates the burst.
            for (dep, prev) in side_effects {
                if !top.side_effects.iter().any(|(d, _)| *d == dep) {
                    top.side_effects.push((dep, prev));
                }
            }
            // A fresh edit still invalidates the redo branch.
            self.redo.clear();
        } else {
            self.push_undo(UndoEntry {
                sheet,
                cell,
                changes: vec![CellChange {
                    cell,
                    before,
                    after,
                }],
                side_effects,
                bulk: false,
            });
        }
        self.last_edit = Some((at, now));

        report.micros = start.elapsed().as_micros();
        report
    }

    /// Record an undoable action. A new action always invalidates the redo
    /// branch — redoing after an edit would replay a history that no longer
    /// exists. The stack is capped at `undo_limit`, dropping the oldest entry
    /// first.
    fn push_undo(&mut self, entry: UndoEntry) {
        // Anything pushed through here that is not the coalescing single-cell
        // path ends a coalescing run.
        if entry.bulk {
            self.last_edit = None;
        }
        self.undo.push(entry);
        self.redo.clear();
        self.enforce_undo_limit();
    }

    /// Evaluate a single formula cell anywhere in the workbook.
    ///
    /// Evaluation goes through [`WorkbookSource`] rather than a bare
    /// `SheetView`, so `Sheet2!A1` inside the formula resolves — including
    /// when the formula itself lives on a parked sheet.
    fn eval_one_at(&mut self, at: SheetCell) {
        let src = match self.overlay_of(at.sheet).and_then(|o| o.get(at.cell)) {
            Some(CellInput::Formula { src, .. }) => src.clone(),
            _ => return,
        };
        let value = match self.parse_on(at.sheet, &src) {
            Ok(expr) => {
                let source = WorkbookSource::new(self, at.sheet);
                eval_view(&expr, &source)
            }
            Err(_) => Value::Error(ErrorKind::Name),
        };
        if let Some(ov) = self.overlay_of_mut(at.sheet) {
            ov.update_cached(at.cell, value);
        }
    }

    /// Recompute every formula in the workbook, in dependency order.
    pub fn recalc_all(&mut self) -> usize {
        match self.graph.full_order_all() {
            Ok(order) => {
                let n = order.len();
                for at in order {
                    self.eval_one_at(at);
                }
                n
            }
            Err(cycle) => {
                let n = cycle.len();
                for at in cycle {
                    if let Some(ov) = self.overlay_of_mut(at.sheet) {
                        ov.update_cached(at.cell, Value::Error(ErrorKind::Circular));
                    }
                }
                n
            }
        }
    }

    // ------------------------------------------------------------- goal seek

    /// Beyond this magnitude a candidate is called divergent and the search
    /// stops.
    ///
    /// This is the guard that makes "a divergent case terminates rather than
    /// spinning" true for the right reason. The iteration cap alone bounds the
    /// wall clock, but a secant search chasing an unreachable target grows its
    /// step geometrically, and long before iteration 100 the candidates are
    /// numerically meaningless — evaluating `=B1*B1` at 1e200 overflows to
    /// infinity and every subsequent step is arithmetic on infinities. Cutting
    /// out at a value no spreadsheet input plausibly holds keeps the reported
    /// "closest value found" a real number the user can read.
    const GOAL_SEEK_DIVERGENCE_LIMIT: f64 = 1e12;

    /// Try one candidate value for the changing cell and read the target back.
    ///
    /// Writes STRAIGHT to the overlay and recalculates only what depends on
    /// the changing cell — no undo entry, no dirty flag, no coalescing state.
    /// See [`Workbook::goal_seek`] for why the search must not go through
    /// `commit_edit`.
    ///
    /// Returns `None` when the target did not evaluate to a number (text, an
    /// error value, or a cycle), which the caller treats as "no gradient to
    /// follow" rather than as a value of zero.
    fn goal_seek_probe(&mut self, at: SheetCell, target: CellRef, x: f64) -> Option<f64> {
        self.overlay
            .set(at.cell, CellInput::Literal(Value::Number(x)));
        // The changing cell is a literal for the whole search (a formula there
        // is refused up front), so it has no precedents of its own to resync —
        // only its dependents need re-evaluating.
        match self.graph.recalc_order_at(at) {
            Ok(order) => {
                for dep in order {
                    self.eval_one_at(dep);
                }
            }
            Err(_) => return None,
        }
        self.view().get(target).as_number()
    }

    /// Restore the changing cell and everything downstream of it to whatever
    /// the overlay held before the search started.
    fn goal_seek_restore(&mut self, at: SheetCell, before: Option<CellInput>) {
        self.overlay.restore(at.cell, before);
        self.resync_graph_at(at);
        if let Ok(order) = self.graph.recalc_order_at(at) {
            for dep in order {
                self.eval_one_at(dep);
            }
        }
    }

    /// Set `target` to `target_value` by changing `changing`. Both cells are
    /// on the ACTIVE sheet. Issue #35.
    ///
    /// Uses the secant method: it needs no derivative, converges fast on the
    /// linear and near-linear models spreadsheets are mostly made of, and
    /// degrades to "no progress" rather than to a wrong answer when the model
    /// is flat.
    ///
    /// ## Why the search does not go through `commit_edit`
    ///
    /// The whole run has to be ONE undo step. The search tries up to
    /// [`GOAL_SEEK_MAX_ITERS`] candidates, and `commit_edit` pushes an undo
    /// entry per call. Coalescing would fold most of them together — same
    /// cell, inside [`COALESCE_WINDOW`] — but that is a timing accident, not a
    /// guarantee: one recalculation slower than a second, entirely plausible
    /// on a large sheet, would silently split the run into two undo steps and
    /// leave the user's first Ctrl+Z landing on an intermediate guess.
    ///
    /// So the search writes candidates straight into the overlay through
    /// [`Workbook::goal_seek_probe`], touching no history at all. When it
    /// finishes it restores the pre-search state exactly, then makes a single
    /// real `commit_edit` of the winning value, bracketed by `end_edit_run` so
    /// it can neither fold into the keystroke before it nor absorb the one
    /// after. That one entry records the original value as its `before` and
    /// every dependent's original cache as its side effects, so undo rewinds
    /// the entire run — which is exactly what the dialog's Cancel button does.
    ///
    /// ## Scale
    ///
    /// Nothing here is per row. The search holds a handful of `f64`s and one
    /// saved `CellInput`; each probe recalculates the dependents of the
    /// changing cell, which is a property of the formula graph, not of the
    /// sheet's height.
    pub fn goal_seek(
        &mut self,
        target: CellRef,
        target_value: f64,
        changing: CellRef,
    ) -> Result<GoalSeekReport, GoalSeekError> {
        let sheet = self.active_sheet();
        let ta = SheetCell::new(sheet, target);
        let ca = SheetCell::new(sheet, changing);

        // THE CHEAP REFUSAL, before a single recalculation. One precedent walk
        // answers "could any value of B move A at all?"; if nothing links them
        // then 100 recalcs would only prove it slowly and then blame the user
        // for a non-convergence that was never their arithmetic's fault.
        if !self.graph.depends_on_at(ta, ca) {
            return Err(GoalSeekError::NotDependent);
        }
        // Overwriting a formula with a number is data loss, and the changing
        // cell's value is not ours to choose when the sheet already computes
        // it. Excel refuses this for the same reason.
        if self.overlay.has_formula(changing) {
            return Err(GoalSeekError::ChangingCellIsFormula);
        }

        let before = self.overlay.get(changing).cloned();
        let x0 = self.view().get(changing).as_number().unwrap_or(0.0);
        // The second sample the secant needs, scaled off the starting value so
        // it is meaningful whether B holds 0.01 or 1e9, with a floor so a
        // starting value of exactly 0 still produces two distinct samples.
        let step = (x0.abs() * 0.01).max(1e-4);

        let mut iterations = 0usize;
        let mut converged = false;
        let mut best_x = x0;
        let mut best_a: Option<f64> = None;
        let mut best_err = f64::INFINITY;
        // The previous (x, f(x)) sample; `None` on the first pass.
        let mut prev: Option<(f64, f64)> = None;
        let mut next = x0;

        for _ in 0..GOAL_SEEK_MAX_ITERS {
            let x = next;
            iterations += 1;
            let Some(a) = self.goal_seek_probe(ca, target, x) else {
                // The target stopped being a number. There is no gradient to
                // follow, so stop and report the best real approach so far
                // rather than iterate on nonsense.
                break;
            };
            let f = a - target_value;
            let err = f.abs();
            // Track the CLOSEST approach, not merely the last one: a
            // non-converged run must report a value it actually reached.
            if err < best_err {
                best_err = err;
                best_x = x;
                best_a = Some(a);
            }
            if err < GOAL_SEEK_EPSILON {
                converged = true;
                break;
            }

            let nx = match prev {
                // First pass: no secant line yet, so take the offset sample.
                None => x + step,
                Some((px, pf)) => {
                    let denom = f - pf;
                    if denom == 0.0 {
                        // Two different inputs, identical output: the target
                        // is flat in this cell over this interval, so there is
                        // no direction to move in. Stop instead of dividing by
                        // zero and chasing an infinity.
                        break;
                    }
                    x - f * (x - px) / denom
                }
            };
            prev = Some((x, f));
            if !nx.is_finite() || nx.abs() > Self::GOAL_SEEK_DIVERGENCE_LIMIT {
                break;
            }
            next = nx;
        }

        // Put the sheet back exactly as the user left it BEFORE recording the
        // one real edit, so `commit_edit` captures the pre-Goal-Seek value as
        // its `before` and the pre-Goal-Seek caches as its side effects. Skip
        // this and undo would rewind to the last probe instead.
        self.goal_seek_restore(ca, before);

        let Some(probe_a) = best_a else {
            // Not one candidate made the target a number, so there is no
            // winning value to commit. The sheet is already restored and the
            // undo stack is untouched.
            return Ok(GoalSeekReport {
                converged: false,
                iterations,
                target: target_value,
                final_b: x0,
                final_a: None,
            });
        };

        // `{}` on f64 prints the shortest representation that parses back to
        // the same bits, so the committed text re-reads as exactly the value
        // that was probed.
        self.end_edit_run();
        self.commit_edit(changing, &format!("{best_x}"));
        self.end_edit_run();

        // Read the target back from the committed state rather than trusting
        // the probe: this is the number the user will actually see.
        let final_a = self.view().get(target).as_number().or(Some(probe_a));

        Ok(GoalSeekReport {
            converged,
            iterations,
            target: target_value,
            final_b: best_x,
            final_a,
        })
    }

    pub fn undo(&mut self) -> Option<CellRef> {
        let sheet = self.undo.last()?.sheet;
        // Show the user what is changing: an undo of an edit on another sheet
        // switches to it rather than silently rewriting an invisible cell.
        let _ = self.activate(sheet);
        let entry = self.undo.pop()?;
        self.dirty = true;
        // An undo ends any coalescing run: the next keystroke must not fold
        // into an entry the user has just stepped away from.
        self.last_edit = None;
        // Reverse order so overlapping writes unwind exactly as they were made.
        for ch in entry.changes.iter().rev() {
            self.overlay.restore(ch.cell, ch.before.clone());
            self.resync_graph_at(SheetCell::new(sheet, ch.cell));
        }
        // Restore dependent caches captured at commit time. These may live on
        // other sheets, so they are addressed by SheetCell.
        for (dep, prev) in &entry.side_effects {
            if let Some(ov) = self.overlay_of_mut(dep.sheet) {
                ov.update_cached(dep.cell, *prev);
            }
        }
        let cell = entry.cell;
        self.redo.push(entry);
        Some(cell)
    }

    pub fn redo(&mut self) -> Option<CellRef> {
        let sheet = self.redo.last()?.sheet;
        let _ = self.activate(sheet);
        let entry = self.redo.pop()?;
        self.dirty = true;
        self.last_edit = None;
        for ch in &entry.changes {
            self.overlay.restore(ch.cell, ch.after.clone());
            self.resync_graph_at(SheetCell::new(sheet, ch.cell));
        }
        // Re-derive dependents rather than trusting stale caches.
        let touched: Vec<SheetCell> = entry
            .changes
            .iter()
            .map(|c| SheetCell::new(sheet, c.cell))
            .collect();
        for at in touched {
            if let Ok(order) = self.graph.recalc_order_at(at) {
                for dep in order {
                    self.eval_one_at(dep);
                }
            }
        }
        let cell = entry.cell;
        self.undo.push(entry);
        Some(cell)
    }

    /// Read a rectangular block as display strings, for the clipboard.
    ///
    /// Bounded by `max_cells`: a user can select an entire 200M-row column,
    /// and materializing that as text would exhaust memory. Returns `None`
    /// when the selection is too large, so the caller can say so rather than
    /// freeze.
    ///
    /// Superseded on the UI path by [`Self::copy_clip_block`], which carries
    /// formats and formulas as well as text (issue #30). Kept because it is
    /// the text-only contract — smaller, with no formatting to resolve — and
    /// its tests pin the bounds behaviour both paths rely on.
    #[allow(dead_code)]
    pub fn copy_block(&self, sel: Selection, max_cells: u64) -> Option<Vec<Vec<String>>> {
        if sel.cell_count() > max_cells {
            return None;
        }
        let view = self.view();
        let (tl, br) = sel.bounds();
        let mut rows = Vec::with_capacity(sel.row_count() as usize);
        for r in tl.row..=br.row {
            let mut row = Vec::with_capacity(sel.col_count() as usize);
            for c in tl.col..=br.col {
                row.push(view.display(CellRef::new(r, c)));
            }
            rows.push(row);
        }
        Some(rows)
    }

    /// Read a rectangular block as a rich clipboard payload (issue #30).
    ///
    /// Carries what TSV cannot: the formula SOURCE TEXT behind each cell, the
    /// number format in effect, and the resolved fill/text/typography. That is
    /// what makes a Ferrix -> clipboard -> Ferrix round trip preserve values,
    /// number formats and styling rather than only the text.
    ///
    /// Bounded by `max_cells` exactly as [`Self::copy_block`] is, and for the
    /// same reason: a user can select a whole 200M-row column.
    ///
    /// `col_widths` is supplied by the caller because widths live in the app's
    /// sizing model, not the workbook's.
    pub fn copy_clip_block(
        &self,
        sel: Selection,
        max_cells: u64,
        col_widths: impl Fn(u32) -> Option<f32>,
    ) -> Option<ferrix_core::clipboard::ClipBlock> {
        use ferrix_core::clipboard::{ClipBlock, ClipCell};

        if sel.cell_count() > max_cells {
            return None;
        }
        let (tl, br) = sel.bounds();
        let rows = sel.row_count() as usize;
        let cols = sel.col_count() as usize;
        let mut block = ClipBlock::new(rows, cols);

        // Style resolution is per COLUMN per frame in the painter, and it is
        // the same bargain here: build each column's rule plan once rather
        // than once per cell, so a tall copy stays linear in cells with no
        // per-cell plan allocation.
        let view = self.view();
        let mut plan = Vec::new();
        for (dc, c) in (tl.col..=br.col).enumerate() {
            self.format.plan(c, &mut plan);
            let needs_text = ferrix_core::SheetFormat::plan_needs_text(&plan);
            for (dr, r) in (tl.row..=br.row).enumerate() {
                let cell = ferrix_core::CellRef::new(r, c);
                let text = view.display(cell);
                let value = view.get(cell);
                // Window-dependent rules (top-N, colour scales) are skipped:
                // they describe the SOURCE sheet's distribution, which is not
                // a property of the copied cells and would be wrong the moment
                // they landed anywhere else. `resolve` degrades correctly on a
                // short `evals` slice, which is exactly this case.
                let style = self.format.resolve(
                    cell,
                    &value,
                    if needs_text { &text } else { "" },
                    &plan,
                    &[],
                );
                block.set(
                    dr,
                    dc,
                    ClipCell {
                        text,
                        formula: self
                            .overlay
                            .get(cell)
                            .and_then(|i| i.formula_src())
                            .map(|s| s.to_string()),
                        format: self.format.number_format(cell).cloned(),
                        style: ferrix_core::ManualStyle {
                            fill: style.fill,
                            text: style.text,
                            typography: style.typography,
                        },
                        // Where this came from, so a pasted formula can be
                        // offset by the distance it actually travelled.
                        origin: Some(cell),
                    },
                );
            }
        }
        for (dc, c) in (tl.col..=br.col).enumerate() {
            block.col_widths[dc] = col_widths(c);
        }
        Some(block)
    }

    /// Apply a Paste Special request as ONE undo step (issue #30).
    ///
    /// Everything the operation touches — every cell, whatever the mode —
    /// goes into a single bulk [`UndoEntry`], so a 100k-cell paste is one
    /// Ctrl+Z exactly like a bulk clear or a Replace All. That is not a new
    /// mechanism: it is the same `bulk: true` entry those already push.
    ///
    /// Formats do NOT go through the undo stack, because they are not cell
    /// contents; they are reported in [`PasteReport::format_rects`] so the
    /// caller can say what happened.
    pub fn paste_special(
        &mut self,
        origin: CellRef,
        block: &ferrix_core::clipboard::ClipBlock,
        opts: ferrix_core::clipboard::PasteOptions,
        max_cells: u64,
    ) -> Result<PasteReport, String> {
        use ferrix_core::clipboard::{PasteWhat, TRANSPOSE_NOTE};

        // Transpose first, so every bound, guard and write below sees the
        // shape that will actually land. Checking the pre-transpose rectangle
        // would guard the wrong cells — the merge check especially.
        let transposed;
        let block = if opts.transpose {
            transposed = block.transposed();
            &transposed
        } else {
            block
        };

        if block.is_empty() {
            return Err("Clipboard is empty".into());
        }
        let cells = block.cell_count();
        if cells > max_cells {
            return Err(format!(
                "pasting {cells} cells exceeds the {max_cells}-cell limit"
            ));
        }
        let rows = block.rows() as u32;
        let cols = block.cols() as u32;
        let dest = Selection::new(
            origin,
            CellRef::new(origin.row + rows - 1, origin.col + cols - 1),
        );

        // Merged regions, BEFORE anything is written. A paste that would
        // overwrite part of a merged region is refused outright rather than
        // half-applied — the same instinct as `MergeError::Overlaps`, and the
        // reason this check precedes the protection guard's own all-or-nothing
        // contract instead of being interleaved with the writes.
        if let Some(region) = self.merge_conflict(dest) {
            return Err(format!(
                "Paste refused — {} would overwrite part of the merged region {}. \
                 Unmerge it first, or paste somewhere else.",
                dest.label(),
                merge_label(region)
            ));
        }

        // Issue #42: same chokepoint as every other bulk write.
        self.guard_range(dest).map_err(|d| d.to_string())?;

        let mut report = PasteReport {
            transposed: opts.transpose,
            note: opts.transpose.then(|| TRANSPOSE_NOTE.to_string()),
            ..Default::default()
        };

        if opts.what.writes_contents() {
            report.cells_written = self.paste_contents(origin, block, opts)?;
        }
        if opts.what.writes_formats() {
            report.format_rects = self.paste_formats(origin, block);
        }
        if opts.what == PasteWhat::ColumnWidths {
            // Widths live in the app's sizing model, so they are handed back
            // rather than applied here. Reported per destination column.
            report.col_widths = (0..block.cols())
                .filter_map(|c| {
                    block
                        .col_widths
                        .get(c)
                        .and_then(|w| *w)
                        .map(|w| (origin.col + c as u32, w))
                })
                .collect();
        }
        Ok(report)
    }

    /// Write the cell contents half of a paste, as one bulk undo entry.
    fn paste_contents(
        &mut self,
        origin: CellRef,
        block: &ferrix_core::clipboard::ClipBlock,
        opts: ferrix_core::clipboard::PasteOptions,
    ) -> Result<usize, String> {
        use ferrix_core::clipboard::PasteWhat;

        let mut changes = Vec::new();
        for r in 0..block.rows() {
            for c in 0..block.cols() {
                let Some(src) = block.get(r, c) else {
                    continue;
                };
                let cell = CellRef::new(origin.row + r as u32, origin.col + c as u32);

                // Skip Blanks: a blank clipboard cell leaves the destination
                // exactly as it was, rather than clearing it. Checked before
                // anything is recorded, so a skipped cell is not in the undo
                // entry at all and an undo cannot "restore" a value that was
                // never overwritten.
                if opts.skip_blanks && src.text.trim().is_empty() && src.formula.is_none() {
                    continue;
                }

                let text = self.paste_cell_text(cell, src, opts);
                let before = self.overlay.get(cell).cloned();
                let after = match &text {
                    Some(t) => self.classify(t),
                    // `None` means the arithmetic refused this pair; leave the
                    // destination untouched rather than writing an error over
                    // data the user did not ask to change.
                    None => continue,
                };
                // Nothing actually changes: don't put it in the undo entry.
                if before == after {
                    continue;
                }
                match &after {
                    Some(input) => {
                        self.overlay.set(cell, input.clone());
                    }
                    None => {
                        self.overlay.clear(cell);
                    }
                }
                self.resync_graph(cell);
                changes.push(CellChange {
                    cell,
                    before,
                    after,
                });
                let _ = PasteWhat::All; // keeps the import honest across cfgs
            }
        }
        if changes.is_empty() {
            return Ok(0);
        }
        let n = changes.len();
        self.dirty = true;
        // ONE entry for the whole paste, `bulk: true` so it never coalesces
        // with a neighbouring keystroke — identical to `paste_block`,
        // `clear_range` and Replace All.
        self.push_undo(UndoEntry {
            sheet: self.active_sheet(),
            cell: origin,
            changes,
            side_effects: Vec::new(),
            bulk: true,
        });
        self.recalc_all();
        Ok(n)
    }

    /// What one destination cell should receive, or `None` to leave it alone.
    fn paste_cell_text(
        &self,
        cell: CellRef,
        src: &ferrix_core::clipboard::ClipCell,
        opts: ferrix_core::clipboard::PasteOptions,
    ) -> Option<String> {
        use ferrix_core::clipboard::{PasteOp, PasteWhat};

        // Arithmetic combines NUMBERS, so it is incompatible with pasting a
        // formula: Excel resolves this the same way, by using the values.
        if opts.op != PasteOp::None {
            let dest = self.view().get(cell).as_number();
            let combined = opts.op.apply(dest, src.as_number())?;
            return Some(ferrix_core::format_number(combined));
        }

        match opts.what {
            // Values: a formula lands as the number it evaluated to.
            PasteWhat::Values => Some(src.text.clone()),
            PasteWhat::All | PasteWhat::Formulas => match &src.formula {
                Some(f) => {
                    // Formula references are rewritten as TEXT for the offset
                    // between where the cell was copied and where it landed.
                    // `$` anchors are honoured — see
                    // `ferrix_formula::paste_formula`.
                    let (drow, dcol) = match src.origin {
                        Some(from) => (
                            cell.row as i64 - from.row as i64,
                            cell.col as i64 - from.col as i64,
                        ),
                        None => (0, 0),
                    };
                    Some(ferrix_formula::paste_formula(f, drow, dcol))
                }
                // Formulas mode over a cell that never held one still writes
                // the value; a mode that silently skipped them would leave
                // holes in the pasted block.
                None => Some(src.text.clone()),
            },
            PasteWhat::Formats | PasteWhat::ColumnWidths => None,
        }
    }

    /// Write the formatting half of a paste.
    ///
    /// **This is where the scale invariant is kept.** The clipboard's per-cell
    /// formats and styles are collapsed into maximal RECTANGLES first, so a
    /// uniform format over a 100k-cell region becomes one
    /// [`ferrix_core::RangeFormat`] entry — not 100k cell overrides, which
    /// would be a per-cell format store by another name and would blow the
    /// invariant `format.rs` exists to protect.
    ///
    /// Returns how many rectangles were stored.
    fn paste_formats(
        &mut self,
        origin: CellRef,
        block: &ferrix_core::clipboard::ClipBlock,
    ) -> usize {
        use ferrix_core::clipboard::merge_rectangles;

        let (rows, cols) = (block.rows(), block.cols());
        let cell_at = |i: usize| block.get(i / cols, i % cols);

        // Number formats, keyed by the format itself so equal neighbours merge.
        let fmt_keys: Vec<Option<ferrix_core::NumberFormat>> = (0..rows * cols)
            .map(|i| cell_at(i).and_then(|c| c.format.clone()))
            .collect();
        let style_keys: Vec<Option<ferrix_core::ManualStyle>> = (0..rows * cols)
            .map(|i| {
                cell_at(i)
                    .map(|c| c.style)
                    .filter(|s: &ferrix_core::ManualStyle| !s.is_empty())
            })
            .collect();

        let mut stored = 0usize;
        for rect in merge_rectangles(&fmt_keys, rows, cols) {
            let Some(fmt) = fmt_keys[rect.key_index].clone() else {
                continue;
            };
            let range = ferrix_core::TableRange::new(
                origin.row + rect.first_row as u32,
                origin.col + rect.first_col as u32,
                origin.row + rect.last_row as u32,
                origin.col + rect.last_col as u32,
            );
            let mut rf = ferrix_core::RangeFormat::new(range);
            rf.format = Some(fmt);
            self.format.push_range(rf);
            stored += 1;
        }
        for rect in merge_rectangles(&style_keys, rows, cols) {
            let Some(style) = style_keys[rect.key_index] else {
                continue;
            };
            let range = ferrix_core::TableRange::new(
                origin.row + rect.first_row as u32,
                origin.col + rect.first_col as u32,
                origin.row + rect.last_row as u32,
                origin.col + rect.last_col as u32,
            );
            self.format.set_range_manual(range, style);
            stored += 1;
        }
        if stored > 0 {
            self.dirty = true;
        }
        stored
    }

    /// The merged region a write over `dest` would partially overwrite.
    ///
    /// "Partially" is the whole point: a paste whose destination exactly
    /// covers a merge is writing the merge as a unit and is fine, while one
    /// that clips a corner off would put a value into a cell that displays as
    /// part of its neighbour — invisible data loss. Only the second is
    /// refused.
    pub fn merge_conflict(&self, dest: Selection) -> Option<ferrix_core::TableRange> {
        let (tl, br) = dest.bounds();
        self.merges
            .regions()
            .find(|r| {
                let intersects = r.first_row <= br.row
                    && tl.row <= r.last_row
                    && r.first_col <= br.col
                    && tl.col <= r.last_col;
                let fully_inside = r.first_row >= tl.row
                    && r.last_row <= br.row
                    && r.first_col >= tl.col
                    && r.last_col <= br.col;
                intersects && !fully_inside
            })
            .copied()
    }

    /// Clear every cell in a selection as ONE undo step.
    pub fn clear_range(&mut self, sel: Selection, max_cells: u64) -> Result<usize, String> {
        if sel.cell_count() > max_cells {
            return Err(format!(
                "{} cells is too many to clear at once (limit {})",
                sel.cell_count(),
                max_cells
            ));
        }
        // Issue #42: same chokepoint discipline as `commit_edit`. All or
        // nothing — see `guard_range`.
        self.guard_range(sel).map_err(|d| d.to_string())?;
        let sheet = self.active_sheet();
        let mut changes = Vec::new();
        for cell in sel.iter() {
            let before = self.overlay.get(cell).cloned();
            // Only record cells that actually change.
            let is_empty_already = before.is_none() && self.view().get(cell) == Value::Empty;
            if is_empty_already {
                continue;
            }
            let after = Some(CellInput::Literal(Value::Empty));
            self.overlay.set(cell, CellInput::Literal(Value::Empty));
            self.graph.remove_at(SheetCell::new(sheet, cell));
            changes.push(CellChange {
                cell,
                before,
                after,
            });
        }
        if changes.is_empty() {
            return Ok(0);
        }
        let n = changes.len();
        self.dirty = true;
        self.push_undo(UndoEntry {
            sheet: self.active_sheet(),
            cell: sel.bounds().0,
            changes,
            side_effects: Vec::new(),
            bulk: true,
        });
        self.recalc_all();
        Ok(n)
    }

    /// Replace across the whole sheet as exactly ONE undo step.
    ///
    /// ## Why this streams
    ///
    /// The candidate cells are produced a row window at a time
    /// ([`SheetView::replace_window`]), never as one list of every match. On a
    /// 200M-row sheet a term matching 80 million cells would otherwise cost
    /// 640 MB of `CellRef`s before a single edit was applied. Here the only
    /// thing that grows with the result is the undo entry — which is exactly
    /// what `max_edits` bounds, derived by the caller from
    /// [`ferrix_core::budget::cost::REPLACE_CELL`].
    ///
    /// ## Why cancellation keeps its edits
    ///
    /// `cancel` is polled at [`ferrix_core::search::CANCEL_POLL_INTERVAL`]
    /// candidates. When it fires the pass stops and everything already written
    /// STAYS written, recorded in the same single undo entry — so one Ctrl+Z
    /// still reverses the whole partial run. Silently rolling back would throw
    /// away work the user watched happen; silently continuing would ignore the
    /// cancel. Reporting the count is what makes the third option honest.
    ///
    /// The undo entry is pushed only if something actually changed, so a
    /// replace that matches nothing leaves the history untouched.
    pub fn replace_all(
        &mut self,
        spec: &ferrix_core::ReplaceSpec,
        max_edits: usize,
        cancel: &ferrix_core::CancelToken,
        mut progress: impl FnMut(usize, usize),
    ) -> ferrix_core::ReplaceReport {
        use ferrix_core::{ReplaceOutcome, ReplaceReport};

        let started = std::time::Instant::now();
        let sheet = self.active_sheet();
        let rows = self.view().row_count();
        let window = self.replace_window_rows.max(1);

        let mut changes: Vec<CellChange> = Vec::new();
        let mut examined = 0usize;
        // Matches left untouched because the cell is protected (issue #42).
        let mut protected_skipped = 0usize;
        let mut outcome = ReplaceOutcome::Completed;
        let mut first_cell: Option<CellRef> = None;

        let mut r0 = 0usize;
        'outer: while r0 < rows {
            let r1 = (r0 + window).min(rows);
            // The borrow lives only as long as the scan. Dropping it before
            // applying is what lets the loop hold `&mut self` at all — and it
            // is also what bounds memory to one window.
            let candidates = self
                .view()
                .replace_window(&spec.query, spec.look_in, r0, r1);
            r0 = r1;

            for (cell, text) in candidates {
                if examined % ferrix_core::search::CANCEL_POLL_INTERVAL == 0 {
                    progress(examined, changes.len());
                    if cancel.is_cancelled() {
                        outcome = ReplaceOutcome::Cancelled;
                        break 'outer;
                    }
                }
                examined += 1;
                let Some(new_text) = spec.rewrite(&text) else {
                    continue;
                };
                // Issue #42. Unlike paste/fill, a Replace All is a SCAN of the
                // whole sheet, so all-or-nothing would mean one locked cell
                // anywhere cancels a legitimate replace everywhere. Protected
                // cells are skipped instead, and counted, so the report can
                // say how many were left alone rather than the user
                // discovering it later.
                if let Some(d) = self.sheets[self.active].protection.deny_edit(cell) {
                    protected_skipped += 1;
                    self.last_denial = Some(d);
                    continue;
                }
                if changes.len() >= max_edits {
                    outcome = ReplaceOutcome::BudgetExhausted;
                    break 'outer;
                }

                let before = self.overlay.get(cell).cloned();
                let after = self.classify(&new_text);
                match &after {
                    Some(input) => {
                        self.overlay.set(cell, input.clone());
                    }
                    None => {
                        self.overlay.clear(cell);
                    }
                }
                // Keeps the dependency graph honest whether the rewrite
                // produced a formula, destroyed one, or left a literal — which
                // is what makes 'look in: formulas' recalculate correctly.
                self.resync_graph(cell);
                first_cell.get_or_insert(cell);
                changes.push(CellChange {
                    cell,
                    before,
                    after,
                });
            }

            // A cancel between windows must still be honoured promptly on a
            // sheet whose windows contain few or no matches. Reporting here
            // too means progress advances with the SCAN, not only with the
            // writes — a long pass over a sheet with sparse matches would
            // otherwise look frozen.
            progress(examined, changes.len());
            if cancel.is_cancelled() {
                outcome = ReplaceOutcome::Cancelled;
                break;
            }
        }
        progress(examined, changes.len());

        let applied = changes.len();
        if applied > 0 {
            self.dirty = true;
            // ONE entry for the whole pass, cancelled or not. `bulk: true`
            // also stops it coalescing with a neighbouring keystroke, matching
            // clear/paste/fill.
            self.push_undo(UndoEntry {
                sheet,
                cell: first_cell.unwrap_or(CellRef::new(0, 0)),
                changes,
                side_effects: Vec::new(),
                bulk: true,
            });
            self.recalc_all();
        }

        ReplaceReport {
            applied,
            examined,
            outcome,
            millis: started.elapsed().as_millis(),
            protected_skipped,
        }
    }

    /// Replace at exactly one cell, as an ordinary single-cell edit.
    ///
    /// Returns the new text when the cell changed. Goes through
    /// `commit_edit`, so it recalculates and undoes like anything the user
    /// typed — a single Replace should not behave differently from typing the
    /// same result by hand.
    pub fn replace_one(
        &mut self,
        cell: CellRef,
        spec: &ferrix_core::ReplaceSpec,
    ) -> Option<String> {
        let text = self.view().replace_text(cell, spec.look_in)?;
        let new_text = spec.rewrite(&text)?;
        // Break any coalescing run: a replace is its own logical action, not a
        // continuation of whatever the user last typed.
        self.end_edit_run();
        self.commit_edit(cell, &new_text);
        self.end_edit_run();
        Some(new_text)
    }

    /// Paste a block of text with its top-left corner at `origin`, as ONE
    /// undo step. Returns how many cells were written.
    ///
    /// Superseded on the UI path by [`Self::paste_special`], which handles
    /// formats, formulas and the Paste Special modes (issue #30). Kept as the
    /// plain-text contract, and because its tests pin the one-undo-entry rule
    /// that `paste_special` deliberately matches rather than reinvents.
    #[allow(dead_code)]
    pub fn paste_block(
        &mut self,
        origin: CellRef,
        block: &[Vec<String>],
        max_cells: u64,
    ) -> Result<usize, String> {
        let cells = block.iter().map(|r| r.len() as u64).sum::<u64>();
        if cells > max_cells {
            return Err(format!(
                "pasting {cells} cells exceeds the {max_cells}-cell limit"
            ));
        }
        // Issue #42. The destination rectangle is the block's extent from the
        // origin, which is what a paste actually writes.
        let rows = block.len().max(1) as u32;
        let cols = block.iter().map(|r| r.len()).max().unwrap_or(1).max(1) as u32;
        let dest = Selection::new(
            origin,
            CellRef::new(origin.row + rows - 1, origin.col + cols - 1),
        );
        self.guard_range(dest).map_err(|d| d.to_string())?;
        let mut changes = Vec::new();
        for (dr, row) in block.iter().enumerate() {
            for (dc, text) in row.iter().enumerate() {
                let cell = CellRef::new(origin.row + dr as u32, origin.col + dc as u32);
                let before = self.overlay.get(cell).cloned();
                let after = self.classify(text);
                match &after {
                    Some(input) => {
                        self.overlay.set(cell, input.clone());
                    }
                    None => {
                        self.overlay.clear(cell);
                    }
                }
                self.resync_graph(cell);
                changes.push(CellChange {
                    cell,
                    before,
                    after,
                });
            }
        }
        if changes.is_empty() {
            return Ok(0);
        }
        let n = changes.len();
        self.dirty = true;
        self.push_undo(UndoEntry {
            sheet: self.active_sheet(),
            cell: origin,
            changes,
            side_effects: Vec::new(),
            bulk: true,
        });
        self.recalc_all();
        Ok(n)
    }

    /// Fill from a source selection into an extended target, as ONE undo step.
    ///
    /// `source` is what the user had selected; `target` is the larger range
    /// after dragging the handle. Cells already inside `source` are untouched.
    ///
    /// Numeric sources of 2+ cells continue their arithmetic progression;
    /// everything else tiles. Formulas have their relative references offset
    /// so `=A1*2` filled down becomes `=A2*2`, while `$` anchors stay put.
    pub fn fill_range(
        &mut self,
        source: Selection,
        target: Selection,
        max_cells: u64,
    ) -> Result<(usize, FillKind), String> {
        if target.cell_count() > max_cells {
            return Err(format!(
                "filling {} cells exceeds the {}-cell limit",
                target.cell_count(),
                max_cells
            ));
        }
        // Issue #42.
        self.guard_range(target).map_err(|d| d.to_string())?;
        let (src_tl, src_br) = source.bounds();
        let (tgt_tl, tgt_br) = target.bounds();

        // Which way is this growing? Only the axis that actually extended.
        let down = tgt_br.row > src_br.row;
        let up = tgt_tl.row < src_tl.row;
        let right = tgt_br.col > src_br.col;
        let left = tgt_tl.col < src_tl.col;
        if !(down || up || right || left) {
            return Ok((0, FillKind::Copy));
        }

        let vertical = down || up;
        let src_len = if vertical {
            source.row_count() as usize
        } else {
            source.col_count() as usize
        };

        // Series detection: read the source values along the fill axis. Only
        // attempted for a single line of cells, since a 2-D series is
        // ambiguous and Excel tiles those too.
        let single_line = if vertical {
            source.col_count() == 1
        } else {
            source.row_count() == 1
        };
        let mut step = None;
        if single_line && src_len >= 2 {
            let view = self.view();
            let vals: Vec<Option<f64>> = if vertical {
                (src_tl.row..=src_br.row)
                    .map(|r| match view.get(CellRef::new(r, src_tl.col)) {
                        Value::Number(n) => Some(n),
                        _ => None,
                    })
                    .collect()
            } else {
                (src_tl.col..=src_br.col)
                    .map(|c| match view.get(CellRef::new(src_tl.row, c)) {
                        Value::Number(n) => Some(n),
                        _ => None,
                    })
                    .collect()
            };
            // A source containing formulas must offset them, not extrapolate
            // their current values.
            let has_formula = if vertical {
                (src_tl.row..=src_br.row)
                    .any(|r| self.overlay.has_formula(CellRef::new(r, src_tl.col)))
            } else {
                (src_tl.col..=src_br.col)
                    .any(|c| self.overlay.has_formula(CellRef::new(src_tl.row, c)))
            };
            if !has_formula {
                step = ferrix_formula::fill::detect_step(&vals);
            }
        }
        let kind = if step.is_some() {
            FillKind::Series
        } else {
            FillKind::Copy
        };

        let mut changes = Vec::new();
        for cell in target.iter() {
            if source.contains(cell) {
                continue;
            }
            // Distance from the source block, along the fill axis.
            let n = if vertical {
                if down {
                    (cell.row - src_br.row) as usize
                } else {
                    (src_tl.row - cell.row) as usize
                }
            } else if right {
                (cell.col - src_br.col) as usize
            } else {
                (src_tl.col - cell.col) as usize
            };
            let forward = down || right;

            let new_input = if let Some(stepv) = step {
                // Continue the progression outward from whichever end we grew.
                let view = self.view();
                let anchor = if forward {
                    if vertical {
                        CellRef::new(src_br.row, src_tl.col)
                    } else {
                        CellRef::new(src_tl.row, src_br.col)
                    }
                } else {
                    // Growing backwards anchors on the top-left corner in both
                    // orientations.
                    CellRef::new(src_tl.row, src_tl.col)
                };
                let base = match view.get(anchor) {
                    Value::Number(v) => v,
                    _ => 0.0,
                };
                let dir = if forward { 1.0 } else { -1.0 };
                Some(CellInput::Literal(Value::Number(
                    base + stepv * dir * n as f64,
                )))
            } else {
                // Tile: pick the corresponding source cell.
                // `n` is 1-based distance from the block edge.
                let idx = ferrix_formula::fill::tile_index(n - 1, src_len);
                let src_cell = if vertical {
                    let r = if forward {
                        src_tl.row + idx as u32
                    } else {
                        src_br.row - idx as u32
                    };
                    CellRef::new(r, cell.col.clamp(src_tl.col, src_br.col))
                } else {
                    let c = if forward {
                        src_tl.col + idx as u32
                    } else {
                        src_br.col - idx as u32
                    };
                    CellRef::new(cell.row.clamp(src_tl.row, src_br.row), c)
                };
                match self.overlay.get(src_cell) {
                    Some(CellInput::Formula { src, .. }) => {
                        let (dr, dc) = ferrix_formula::fill::delta(src_cell, cell);
                        let rewritten = ferrix_formula::fill::offset_formula(src, dr, dc);
                        Some(
                            self.classify(&rewritten)
                                .unwrap_or(CellInput::Literal(Value::Error(ErrorKind::Name))),
                        )
                    }
                    _ => {
                        let v = self.view().get(src_cell);
                        Some(CellInput::Literal(v))
                    }
                }
            };

            let before = self.overlay.get(cell).cloned();
            if let Some(input) = &new_input {
                self.overlay.set(cell, input.clone());
            }
            self.resync_graph(cell);
            changes.push(CellChange {
                cell,
                before,
                after: new_input,
            });
        }

        if changes.is_empty() {
            return Ok((0, kind));
        }
        let n = changes.len();
        self.dirty = true;
        self.push_undo(UndoEntry {
            sheet: self.active_sheet(),
            cell: tgt_tl,
            changes,
            side_effects: Vec::new(),
            bulk: true,
        });
        self.recalc_all();
        Ok((n, kind))
    }

    /// Keep the graph consistent with whatever the overlay now holds at `cell`.
    fn resync_graph(&mut self, cell: CellRef) {
        self.resync_graph_at(SheetCell::new(self.active_sheet(), cell));
    }

    /// Sheet-aware `resync_graph`.
    fn resync_graph_at(&mut self, at: SheetCell) {
        let src = self
            .overlay_of(at.sheet)
            .and_then(|o| o.get(at.cell))
            .and_then(|i| i.formula_src())
            .map(|s| s.to_string());
        match src {
            Some(src) => {
                match self.parse_on(at.sheet, &src) {
                    Ok(expr) => {
                        // Resolve names against the CURRENT sheet list, so a
                        // reference to a sheet that no longer exists drops out of
                        // the graph instead of dangling.
                        let resolve = self.name_resolver();
                        self.graph.set_formula_at(at, &expr, &resolve);
                    }
                    Err(_) => self.graph.remove_at(at),
                }
                // After the match, because the Err arm's `remove_at` clears
                // them: a formula naming something undefined must still be
                // findable when that name is later defined.
                self.graph.set_name_uses(at, &src);
            }
            None => self.graph.remove_at(at),
        }
    }

    /// Read-only access to any sheet's overlay.
    fn overlay_of(&self, id: SheetId) -> Option<&EditOverlay> {
        if id == self.sheets[self.active].id {
            Some(&self.overlay)
        } else {
            self.parked.get(&id).map(|(_, o)| o)
        }
    }

    fn overlay_of_mut(&mut self, id: SheetId) -> Option<&mut EditOverlay> {
        if id == self.sheets[self.active].id {
            Some(&mut self.overlay)
        } else {
            self.parked.get_mut(&id).map(|(_, o)| o)
        }
    }
}

/// Evaluation adapter that can see the WHOLE workbook.
///
/// `home` is the sheet an unqualified `A1` means; `Sheet2!A1` is routed by
/// name through the workbook. This is what makes cross-sheet formulas evaluate
/// without the evaluator itself knowing what a workbook is — it only ever sees
/// the [`CellSource`] trait.
pub struct WorkbookSource<'a> {
    wb: &'a Workbook,
    home: SheetId,
}

impl<'a> WorkbookSource<'a> {
    pub fn new(wb: &'a Workbook, home: SheetId) -> Self {
        Self { wb, home }
    }

    fn home_view(&self) -> SheetView<'a> {
        self.wb.sheet_view(self.home).expect("home sheet exists")
    }

    fn named(&self, sheet: &str) -> Option<SheetView<'a>> {
        let id = self.wb.sheet_id_by_name(sheet)?;
        self.wb.sheet_view(id)
    }
}

impl ferrix_formula::CellSource for WorkbookSource<'_> {
    fn get(&self, cell: CellRef) -> Value {
        self.home_view().get(cell)
    }

    fn resolve(&self, id: ferrix_core::StrId) -> &str {
        // Borrow-lifetime note: `SheetView` borrows the workbook, not self, so
        // resolving through a temporary view would not outlive this call.
        // Resolve directly against the home sheet's two arenas instead, in the
        // same overlay-then-base order `SheetView::resolve` uses.
        let home = self.home;
        let overlay = self.wb.overlay_of(home).expect("home sheet exists");
        match overlay.resolve(id) {
            Some(s) => s,
            None => {
                if home == self.wb.sheets[self.wb.active].id {
                    self.wb.base.resolve(id)
                } else {
                    self.wb.parked[&home].0.resolve(id)
                }
            }
        }
    }

    fn sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        self.home_view().sum_rect(start, end)
    }

    fn count_rect(&self, start: CellRef, end: CellRef) -> usize {
        self.home_view().count_rect(start, end)
    }

    fn row_count(&self) -> usize {
        self.home_view().row_count()
    }

    fn get_in(&self, sheet: &str, cell: CellRef) -> Value {
        match self.named(sheet) {
            Some(v) => v.get(cell),
            None => Value::Error(ErrorKind::Ref),
        }
    }

    fn has_sheet(&self, sheet: &str) -> bool {
        self.wb.sheet_id_by_name(sheet).is_some()
    }

    fn sum_rect_in(&self, sheet: &str, start: CellRef, end: CellRef) -> Option<f64> {
        Some(self.named(sheet)?.sum_rect(start, end))
    }

    fn count_rect_in(&self, sheet: &str, start: CellRef, end: CellRef) -> Option<usize> {
        Some(self.named(sheet)?.count_rect(start, end))
    }

    fn row_count_in(&self, sheet: &str) -> Option<usize> {
        Some(self.named(sheet)?.row_count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::Sheet;

    fn wb() -> Workbook {
        let mut s = Sheet::new("t");
        for r in 0..10u32 {
            s.set(CellRef::new(r, 0), Value::Number((r + 1) as f64));
        }
        Workbook::new(BaseData::Memory(s))
    }

    /// Same base as `wb()`, for tests that supply their own overlay.
    fn base_for_test() -> BaseData {
        let mut s = Sheet::new("t");
        for r in 0..10u32 {
            s.set(CellRef::new(r, 0), Value::Number((r + 1) as f64));
        }
        BaseData::Memory(s)
    }

    fn val(w: &Workbook, r: u32, c: u32) -> Value {
        w.view().get(CellRef::new(r, c))
    }

    #[test]
    fn commits_literal_number() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "42.5");
        assert_eq!(val(&w, 0, 1), Value::Number(42.5));
    }

    #[test]
    fn commits_bool_and_text() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "TRUE");
        assert_eq!(val(&w, 0, 1), Value::Bool(true));
        w.commit_edit(CellRef::new(1, 1), "hello");
        assert_eq!(w.view().display(CellRef::new(1, 1)), "hello");
    }

    #[test]
    fn empty_input_clears_cell() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        w.commit_edit(c, "5");
        assert_eq!(val(&w, 0, 1), Value::Number(5.0));
        w.commit_edit(c, "   ");
        assert_eq!(val(&w, 0, 1), Value::Empty);
    }

    #[test]
    fn editing_base_cell_shadows_it() {
        let mut w = wb();
        assert_eq!(val(&w, 0, 0), Value::Number(1.0));
        w.commit_edit(CellRef::new(0, 0), "999");
        assert_eq!(val(&w, 0, 0), Value::Number(999.0));
        // The base itself is untouched — critical for mmap'd files.
        assert_eq!(w.base.get(CellRef::new(0, 0)), Value::Number(1.0));
    }

    #[test]
    fn commits_and_evaluates_formula() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=SUM(A1:A10)");
        assert_eq!(val(&w, 0, 1), Value::Number(55.0));
    }

    #[test]
    fn formula_recalculates_when_input_changes() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=A1*2");
        assert_eq!(val(&w, 0, 1), Value::Number(2.0));
        // Change A1; B1 must follow.
        let rep = w.commit_edit(CellRef::new(0, 0), "50");
        assert_eq!(val(&w, 0, 1), Value::Number(100.0));
        assert_eq!(rep.recalculated, 1);
    }

    #[test]
    fn chained_formulas_recalc_in_order() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=A1*2"); // B1 = 2
        w.commit_edit(CellRef::new(0, 2), "=B1+1"); // C1 = 3
        assert_eq!(val(&w, 0, 2), Value::Number(3.0));

        let rep = w.commit_edit(CellRef::new(0, 0), "10");
        // B1 = 20, C1 = 21 — C1 must have seen the NEW B1, not the stale one.
        assert_eq!(val(&w, 0, 1), Value::Number(20.0));
        assert_eq!(val(&w, 0, 2), Value::Number(21.0));
        assert_eq!(rep.recalculated, 2);
    }

    #[test]
    fn formula_over_edited_range_is_correct() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=SUM(A1:A10)");
        assert_eq!(val(&w, 0, 1), Value::Number(55.0));
        // Edit a cell inside the summed range.
        w.commit_edit(CellRef::new(0, 0), "101"); // was 1
        assert_eq!(val(&w, 0, 1), Value::Number(155.0));
    }

    #[test]
    fn self_reference_is_flagged_circular() {
        let mut w = wb();
        let rep = w.commit_edit(CellRef::new(0, 1), "=B1+1");
        assert!(rep.circular);
        assert_eq!(val(&w, 0, 1), Value::Error(ErrorKind::Circular));
    }

    #[test]
    fn mutual_cycle_is_flagged() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=C1+1");
        let rep = w.commit_edit(CellRef::new(0, 2), "=B1+1");
        assert!(rep.circular);
    }

    #[test]
    fn malformed_formula_reports_error_not_panic() {
        let mut w = wb();
        let rep = w.commit_edit(CellRef::new(0, 1), "=SUM(");
        assert!(rep.parse_error.is_some());
        assert!(val(&w, 0, 1).is_error());
    }

    #[test]
    fn undo_restores_previous_value() {
        let mut w = wb();
        let c = CellRef::new(0, 0);
        w.commit_edit(c, "999");
        assert_eq!(val(&w, 0, 0), Value::Number(999.0));
        w.undo();
        // Back to the base value, with no overlay entry left behind.
        assert_eq!(val(&w, 0, 0), Value::Number(1.0));
        assert_eq!(w.edit_count(), 0);
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        w.commit_edit(c, "7");
        w.undo();
        assert_eq!(val(&w, 0, 1), Value::Empty);
        w.redo();
        assert_eq!(val(&w, 0, 1), Value::Number(7.0));
    }

    #[test]
    fn undo_restores_dependent_formulas() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=A1*2"); // B1 = 2
        w.commit_edit(CellRef::new(0, 0), "50"); // B1 -> 100
        assert_eq!(val(&w, 0, 1), Value::Number(100.0));
        w.undo(); // undo the A1 edit
        assert_eq!(val(&w, 0, 0), Value::Number(1.0));
        // B1 must go back to 2, not stay at the stale 100.
        assert_eq!(val(&w, 0, 1), Value::Number(2.0));
    }

    #[test]
    fn multi_level_undo() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        // `end_edit_run` between each keeps these three distinct logical
        // edits rather than one coalesced burst — this test is about undo
        // depth, not coalescing (which has its own tests below).
        w.commit_edit(c, "1");
        w.end_edit_run();
        w.commit_edit(c, "2");
        w.end_edit_run();
        w.commit_edit(c, "3");
        assert_eq!(val(&w, 0, 1), Value::Number(3.0));
        w.undo();
        assert_eq!(val(&w, 0, 1), Value::Number(2.0));
        w.undo();
        assert_eq!(val(&w, 0, 1), Value::Number(1.0));
        w.undo();
        assert_eq!(val(&w, 0, 1), Value::Empty);
        assert!(!w.can_undo());
    }

    #[test]
    fn new_edit_clears_redo_branch() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        w.commit_edit(c, "1");
        w.undo();
        assert!(w.can_redo());
        w.commit_edit(c, "2");
        assert!(!w.can_redo(), "a fresh edit must invalidate redo");
    }

    #[test]
    fn replacing_formula_with_literal_drops_dependency() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=A1*2");
        assert_eq!(w.graph.len(), 1);
        w.commit_edit(CellRef::new(0, 1), "5");
        assert_eq!(w.graph.len(), 0, "literal must not stay in the dep graph");
        // Changing A1 no longer touches B1.
        w.commit_edit(CellRef::new(0, 0), "77");
        assert_eq!(val(&w, 0, 1), Value::Number(5.0));
    }

    #[test]
    fn editing_far_row_is_cheap_and_correct() {
        // Simulates editing deep into a huge file: the overlay must address it
        // exactly and stay small.
        let mut w = wb();
        let deep = CellRef::new(150_000_000, 3);
        w.commit_edit(deep, "42");
        assert_eq!(w.view().get(deep), Value::Number(42.0));
        assert_eq!(w.edit_count(), 1);
        assert!(w.view().row_count() >= 150_000_001);
    }

    #[test]
    fn dirty_flag_tracks_edits_and_saves() {
        let mut w = wb();
        assert!(!w.is_dirty(), "a freshly opened workbook is clean");
        w.commit_edit(CellRef::new(0, 1), "5");
        assert!(w.is_dirty(), "an edit makes it dirty");
        w.mark_saved();
        assert!(!w.is_dirty());
        w.undo();
        assert!(w.is_dirty(), "undo changes the document too");
    }

    #[test]
    fn restored_formulas_recalculate_against_the_base() {
        // A sidecar stores formula SOURCE plus a cached value. Trusting the
        // cache blindly would let a cell display a number that no longer
        // matches the data underneath it.
        let mut overlay = EditOverlay::new();
        overlay.set(
            CellRef::new(0, 1),
            CellInput::Formula {
                src: "=SUM(A1:A3)".into(),
                cached: Value::Number(999.0),
            },
        );
        let mut w = Workbook::new(base_for_test()).with_overlay(overlay);
        w.rebuild_graph_and_recalc();
        assert_eq!(
            val(&w, 0, 1),
            Value::Number(6.0),
            "stale cached value must be recomputed, not trusted"
        );
        assert!(!w.is_dirty(), "restoring is not an edit");
    }

    #[test]
    fn restored_formula_chain_evaluates_in_order() {
        // C1 depends on B1, which is itself a formula. Evaluating out of
        // order would leave C1 reading a stale value.
        let mut overlay = EditOverlay::new();
        overlay.set(
            CellRef::new(0, 1),
            CellInput::Formula {
                src: "=A1*2".into(),
                cached: Value::Number(0.0),
            },
        );
        overlay.set(
            CellRef::new(0, 2),
            CellInput::Formula {
                src: "=B1+5".into(),
                cached: Value::Number(0.0),
            },
        );
        let mut w = Workbook::new(base_for_test()).with_overlay(overlay);
        w.rebuild_graph_and_recalc();
        assert_eq!(val(&w, 0, 1), Value::Number(2.0), "A1 is 1, doubled");
        assert_eq!(
            val(&w, 0, 2),
            Value::Number(7.0),
            "C1 must see B1 recomputed, not its stale cache"
        );
    }

    fn sel(r0: u32, c0: u32, r1: u32, c1: u32) -> Selection {
        Selection::new(CellRef::new(r0, c0), CellRef::new(r1, c1))
    }

    #[test]
    fn clearing_a_range_is_one_undo_step() {
        // The acceptance criterion from the issue: deleting a block must be a
        // single undo, not one per cell.
        let mut w = wb();
        for r in 0..5u32 {
            w.commit_edit(CellRef::new(r, 1), &format!("{r}"));
        }
        let undos_before = w.undo_depth();

        let cleared = w.clear_range(sel(0, 1, 4, 1), 1_000_000).unwrap();
        assert_eq!(cleared, 5);
        assert_eq!(
            w.undo_depth(),
            undos_before + 1,
            "five cleared cells must push exactly one undo entry"
        );

        // One undo restores all five.
        w.undo();
        for r in 0..5u32 {
            assert_eq!(
                val(&w, r, 1),
                Value::Number(r as f64),
                "row {r} not restored by a single undo"
            );
        }
        // And redo re-clears all five.
        w.redo();
        for r in 0..5u32 {
            assert_eq!(val(&w, r, 1), Value::Empty);
        }
    }

    #[test]
    fn copy_block_reads_display_text() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "hello");
        w.commit_edit(CellRef::new(1, 1), "42");
        let block = w.copy_block(sel(0, 0, 1, 1), 1_000_000).unwrap();
        assert_eq!(block.len(), 2);
        assert_eq!(block[0][0], "1", "base column A survives");
        assert_eq!(block[0][1], "hello");
        assert_eq!(block[1][1], "42");
    }

    #[test]
    fn copy_refuses_an_absurd_selection() {
        // Selecting a whole 200M-row column must not try to build 200M strings.
        let w = wb();
        let huge = sel(0, 0, 199_999_999, 7);
        assert!(huge.cell_count() > 1_000_000);
        assert!(
            w.copy_block(huge, 1_000_000).is_none(),
            "an oversized copy must be refused, not attempted"
        );
    }

    #[test]
    fn paste_writes_a_block_as_one_undo_step() {
        let mut w = wb();
        let block = vec![
            vec!["1".to_string(), "two".to_string()],
            vec!["3".to_string(), "four".to_string()],
        ];
        let before = w.undo_depth();
        let n = w
            .paste_block(CellRef::new(0, 1), &block, 1_000_000)
            .unwrap();
        assert_eq!(n, 4);
        assert_eq!(w.undo_depth(), before + 1, "one undo entry for the paste");

        assert_eq!(val(&w, 0, 1), Value::Number(1.0));
        assert_eq!(w.view().display(CellRef::new(0, 2)), "two");
        assert_eq!(val(&w, 1, 1), Value::Number(3.0));

        w.undo();
        assert_eq!(val(&w, 0, 1), Value::Empty, "paste fully undone");
        assert_eq!(val(&w, 1, 1), Value::Empty);
    }

    #[test]
    fn pasted_formulas_evaluate() {
        let mut w = wb();
        let block = vec![vec!["=A1+A2".to_string()]];
        w.paste_block(CellRef::new(0, 1), &block, 1_000_000)
            .unwrap();
        // Base A1=1, A2=2.
        assert_eq!(val(&w, 0, 1), Value::Number(3.0));
    }

    #[test]
    fn paste_refuses_an_oversized_block() {
        let mut w = wb();
        let block = vec![vec!["x".to_string(); 100]; 100];
        let err = w.paste_block(CellRef::new(0, 0), &block, 500).unwrap_err();
        assert!(err.contains("exceeds"), "got: {err}");
    }

    #[test]
    fn fill_down_continues_a_numeric_series() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "1");
        w.commit_edit(CellRef::new(1, 1), "2");
        let src = sel(0, 1, 1, 1);
        let tgt = sel(0, 1, 4, 1);
        let (n, kind) = w.fill_range(src, tgt, 1_000_000).unwrap();
        assert_eq!(n, 3, "three new cells");
        assert_eq!(kind, FillKind::Series);
        assert_eq!(val(&w, 2, 1), Value::Number(3.0));
        assert_eq!(val(&w, 3, 1), Value::Number(4.0));
        assert_eq!(val(&w, 4, 1), Value::Number(5.0));
    }

    #[test]
    fn fill_down_tiles_a_single_value() {
        // One cell is ambiguous as a series, so Excel copies. So do we.
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "7");
        let (n, kind) = w
            .fill_range(sel(0, 1, 0, 1), sel(0, 1, 3, 1), 1_000_000)
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(kind, FillKind::Copy);
        for r in 1..=3u32 {
            assert_eq!(val(&w, r, 1), Value::Number(7.0), "row {r}");
        }
    }

    #[test]
    fn fill_offsets_relative_formula_refs() {
        // The hard part of the issue: =A1*2 filled down must become =A2*2.
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=A1*2");
        assert_eq!(val(&w, 0, 1), Value::Number(2.0), "base A1 is 1");
        let (n, _) = w
            .fill_range(sel(0, 1, 0, 1), sel(0, 1, 2, 1), 1_000_000)
            .unwrap();
        assert_eq!(n, 2);
        // Base column A is 1,2,3 -> doubled 2,4,6.
        assert_eq!(val(&w, 1, 1), Value::Number(4.0));
        assert_eq!(val(&w, 2, 1), Value::Number(6.0));
        // And the stored source really was rewritten, not just the value.
        assert_eq!(
            w.overlay.get(CellRef::new(2, 1)).unwrap().formula_src(),
            Some("=A3*2")
        );
    }

    #[test]
    fn fill_pins_absolute_formula_refs() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=$A$1*2");
        w.fill_range(sel(0, 1, 0, 1), sel(0, 1, 2, 1), 1_000_000)
            .unwrap();
        // $A$1 never moves, so every filled cell is 1*2.
        assert_eq!(val(&w, 1, 1), Value::Number(2.0));
        assert_eq!(val(&w, 2, 1), Value::Number(2.0));
        assert_eq!(
            w.overlay.get(CellRef::new(2, 1)).unwrap().formula_src(),
            Some("=$A$1*2")
        );
    }

    #[test]
    fn fill_is_one_undo_step() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "1");
        w.commit_edit(CellRef::new(1, 1), "2");
        let before = w.undo_depth();
        w.fill_range(sel(0, 1, 1, 1), sel(0, 1, 9, 1), 1_000_000)
            .unwrap();
        assert_eq!(w.undo_depth(), before + 1, "one entry for the whole fill");
        w.undo();
        for r in 2..=9u32 {
            assert_eq!(val(&w, r, 1), Value::Empty, "row {r} restored by one undo");
        }
    }

    #[test]
    fn fill_right_continues_across_columns() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "10");
        w.commit_edit(CellRef::new(0, 2), "20");
        let (_, kind) = w
            .fill_range(sel(0, 1, 0, 2), sel(0, 1, 0, 4), 1_000_000)
            .unwrap();
        assert_eq!(kind, FillKind::Series);
        assert_eq!(val(&w, 0, 3), Value::Number(30.0));
        assert_eq!(val(&w, 0, 4), Value::Number(40.0));
    }

    #[test]
    fn fill_refuses_an_oversized_target() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "1");
        let huge = sel(0, 1, 199_999_999, 1);
        let err = w.fill_range(sel(0, 1, 0, 1), huge, 1_000_000).unwrap_err();
        assert!(err.contains("exceeds"), "got: {err}");
    }

    #[test]
    fn filling_text_tiles_it() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "north");
        let (_, kind) = w
            .fill_range(sel(0, 1, 0, 1), sel(0, 1, 2, 1), 1_000_000)
            .unwrap();
        assert_eq!(kind, FillKind::Copy);
        assert_eq!(w.view().display(CellRef::new(2, 1)), "north");
    }

    // --- bounded history, coalescing, and save behaviour (issue #2) ---

    #[test]
    fn undo_stack_is_bounded_and_drops_the_oldest() {
        let mut w = wb();
        w.set_undo_limit(5);
        // 20 edits to 20 different cells, each its own undo entry.
        for r in 0..20u32 {
            w.commit_edit(CellRef::new(r, 1), &format!("{r}"));
        }
        assert_eq!(w.undo_depth(), 5, "stack must be capped at the limit");

        // The five surviving entries are the NEWEST ones: rows 15..19.
        for r in (15..20u32).rev() {
            assert_eq!(val(&w, r, 1), Value::Number(r as f64));
            w.undo();
            assert_eq!(val(&w, r, 1), Value::Empty, "row {r} was undoable");
        }
        assert!(!w.can_undo(), "nothing older than the cap survived");
        // The dropped edits are still applied — capping loses the ability to
        // undo them, not the edits themselves.
        assert_eq!(val(&w, 0, 1), Value::Number(0.0));
    }

    #[test]
    fn default_undo_limit_is_five_hundred() {
        let w = wb();
        assert_eq!(w.undo_limit(), DEFAULT_UNDO_LIMIT);
        assert_eq!(DEFAULT_UNDO_LIMIT, 500);
    }

    #[test]
    fn lowering_the_limit_trims_existing_history() {
        let mut w = wb();
        for r in 0..10u32 {
            w.commit_edit(CellRef::new(r, 1), "1");
        }
        assert_eq!(w.undo_depth(), 10);
        w.set_undo_limit(3);
        assert_eq!(w.undo_depth(), 3);
    }

    #[test]
    fn rapid_edits_to_one_cell_collapse_to_one_undo_step() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        let before = w.undo_depth();
        // Three edits in immediate succession — well inside COALESCE_WINDOW.
        w.commit_edit(c, "1");
        w.commit_edit(c, "12");
        w.commit_edit(c, "123");
        assert_eq!(
            w.undo_depth(),
            before + 1,
            "a typing burst on one cell is one undo step"
        );
        assert_eq!(val(&w, 0, 1), Value::Number(123.0));
        // One undo rewinds the whole burst, back to before it started.
        w.undo();
        assert_eq!(val(&w, 0, 1), Value::Empty);
        // And redo replays it as one step too.
        w.redo();
        assert_eq!(val(&w, 0, 1), Value::Number(123.0));
    }

    #[test]
    fn rapid_edits_to_different_cells_do_not_coalesce() {
        let mut w = wb();
        let before = w.undo_depth();
        w.commit_edit(CellRef::new(0, 1), "1");
        w.commit_edit(CellRef::new(0, 2), "2");
        w.commit_edit(CellRef::new(0, 3), "3");
        assert_eq!(
            w.undo_depth(),
            before + 3,
            "coalescing must never cross cells"
        );
    }

    #[test]
    fn edits_separated_by_the_window_do_not_coalesce() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        w.commit_edit(c, "1");
        std::thread::sleep(COALESCE_WINDOW + std::time::Duration::from_millis(50));
        w.commit_edit(c, "2");
        assert_eq!(w.undo_depth(), 2, "a pause ends the coalescing run");
    }

    #[test]
    fn leaving_the_cell_ends_the_coalescing_run() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        w.commit_edit(c, "1");
        w.end_edit_run();
        w.commit_edit(c, "2");
        assert_eq!(w.undo_depth(), 2);
    }

    #[test]
    fn a_bulk_op_is_never_coalesced_into() {
        // A paste immediately after typing must stay its own undo step, and a
        // subsequent keystroke must not fold into the paste.
        let mut w = wb();
        let c = CellRef::new(0, 1);
        w.commit_edit(c, "1");
        w.paste_block(c, &[vec!["9".into(), "8".into()]], 1_000_000)
            .unwrap();
        assert_eq!(w.undo_depth(), 2, "paste is its own step");
        w.commit_edit(c, "5");
        assert_eq!(w.undo_depth(), 3, "a keystroke never folds into a paste");
    }

    #[test]
    fn coalesced_burst_still_restores_dependent_formulas() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=A1*2"); // B1 = 2
        w.end_edit_run();
        let a1 = CellRef::new(0, 0);
        w.commit_edit(a1, "50"); // B1 -> 100
        w.commit_edit(a1, "60"); // coalesces; B1 -> 120
        assert_eq!(val(&w, 0, 1), Value::Number(120.0));
        w.undo();
        assert_eq!(val(&w, 0, 0), Value::Number(1.0));
        assert_eq!(
            val(&w, 0, 1),
            Value::Number(2.0),
            "undo must restore the value from before the burst, not mid-burst"
        );
    }

    #[test]
    fn undo_ends_the_coalescing_run() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        w.commit_edit(c, "1");
        w.undo();
        w.commit_edit(c, "2");
        assert_eq!(w.undo_depth(), 1);
        assert!(!w.can_redo(), "a fresh edit still invalidates redo");
    }

    #[test]
    fn saving_clears_undo_history() {
        // DOCUMENTED BEHAVIOUR (README, "Editing"): undo history does not
        // survive a save. The sidecar stores the overlay, not a timeline, so
        // undoing past a save would desync the screen from the file. The
        // count returned lets the UI say so out loud.
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "1");
        w.end_edit_run();
        w.commit_edit(CellRef::new(0, 2), "2");
        w.undo();
        assert!(w.can_undo() && w.can_redo());

        let lost = w.save_committed();
        assert_eq!(lost, 1, "reports how many undo steps were discarded");
        assert!(!w.is_dirty(), "save_committed marks the workbook clean");
        assert!(!w.can_undo(), "undo history is cleared on save");
        assert!(!w.can_redo(), "redo history is cleared on save");
        assert_eq!(w.undo_depth(), 0);

        // The edits themselves survive — only the history is gone.
        assert_eq!(val(&w, 0, 1), Value::Number(1.0));

        // And editing after a save starts a fresh history.
        w.commit_edit(CellRef::new(0, 3), "3");
        assert_eq!(w.undo_depth(), 1);
    }

    #[test]
    fn save_does_not_coalesce_across_the_boundary() {
        let mut w = wb();
        let c = CellRef::new(0, 1);
        w.commit_edit(c, "1");
        w.save_committed();
        w.commit_edit(c, "2");
        assert_eq!(w.undo_depth(), 1, "post-save edit starts a new entry");
        w.undo();
        assert_eq!(
            val(&w, 0, 1),
            Value::Number(1.0),
            "undo rewinds to the saved state, not past it"
        );
    }

    // --- multi-sheet workbooks (issue #15) ---

    /// A workbook with `Sheet1` (base A1:A10 = 1..10) plus an empty `Sheet2`.
    fn two_sheet_wb() -> (Workbook, SheetId) {
        let mut w = wb();
        let s2 = w
            .add_sheet("Sheet2", BaseData::Memory(Sheet::new("Sheet2")))
            .expect("add");
        (w, s2)
    }

    fn val_in(w: &Workbook, sheet: SheetId, r: u32, c: u32) -> Value {
        w.sheet_view(sheet)
            .expect("sheet exists")
            .get(CellRef::new(r, c))
    }

    #[test]
    fn a_fresh_workbook_has_exactly_one_sheet() {
        let w = wb();
        assert_eq!(w.sheet_count(), 1);
        assert_eq!(w.active_name(), "Sheet1");
        // The lone sheet is MAIN, which is what keeps single-sheet graphs
        // byte-identical to the pre-sheets behaviour.
        assert_eq!(w.active_sheet(), SheetId::MAIN);
    }

    #[test]
    fn sheets_are_added_switched_and_stay_independent() {
        let (mut w, s2) = two_sheet_wb();
        assert_eq!(w.sheet_count(), 2);
        // Sheet1's base data is NOT visible from Sheet2 — separate storage.
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 0), Value::Number(1.0));
        assert_eq!(val_in(&w, s2, 0, 0), Value::Empty);

        w.activate(s2).unwrap();
        assert_eq!(w.active_name(), "Sheet2");
        w.commit_edit(CellRef::new(0, 0), "99");
        assert_eq!(val_in(&w, s2, 0, 0), Value::Number(99.0));
        // Editing Sheet2 must not touch Sheet1.
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 0), Value::Number(1.0));
    }

    #[test]
    fn duplicate_sheet_names_are_refused() {
        let (mut w, s2) = two_sheet_wb();
        let err = w
            .add_sheet("Sheet1", BaseData::Memory(Sheet::new("x")))
            .unwrap_err();
        assert_eq!(err, SheetError::DuplicateName("Sheet1".into()));
        // Case-insensitively, too: Excel would not let you have both.
        assert!(w
            .add_sheet("SHEET1", BaseData::Memory(Sheet::new("x")))
            .is_err());
        // And renaming onto an existing name is refused as well.
        assert!(w.rename_sheet(s2, "Sheet1").is_err());
        // A blank name is not a name.
        assert_eq!(
            w.rename_sheet(s2, "   ").unwrap_err(),
            SheetError::EmptyName
        );
        assert_eq!(w.sheet_count(), 2, "no failed add left a sheet behind");
    }

    #[test]
    fn renaming_a_sheet_to_its_own_name_is_allowed() {
        let (mut w, s2) = two_sheet_wb();
        assert!(w.rename_sheet(s2, "Sheet2").is_ok());
        assert!(w.rename_sheet(s2, "Summary").is_ok());
        assert_eq!(w.sheet_name(s2), Some("Summary"));
    }

    #[test]
    fn sheets_can_be_reordered_without_changing_the_active_one() {
        let (mut w, s2) = two_sheet_wb();
        assert_eq!(w.sheet_names()[0].0, SheetId::MAIN);
        w.reorder_sheet(s2, 0).unwrap();
        assert_eq!(w.sheet_names()[0].0, s2);
        assert_eq!(w.sheet_names()[1].0, SheetId::MAIN);
        // Reordering is presentation only.
        assert_eq!(w.active_sheet(), SheetId::MAIN);
        assert_eq!(w.active_index(), 1, "the active sheet moved position");
    }

    #[test]
    fn the_last_sheet_cannot_be_deleted() {
        let mut w = wb();
        assert_eq!(
            w.delete_sheet(SheetId::MAIN).unwrap_err(),
            SheetError::LastSheet
        );
        assert_eq!(w.sheet_count(), 1);
    }

    #[test]
    fn deleting_the_active_sheet_falls_back_to_a_neighbour() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 0), "5");
        w.delete_sheet(s2).unwrap();
        assert_eq!(w.sheet_count(), 1);
        assert_eq!(w.active_sheet(), SheetId::MAIN);
        // The surviving sheet's data is intact and readable.
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 0), Value::Number(1.0));
    }

    #[test]
    fn cross_sheet_reference_parses_and_evaluates() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 0), "7"); // Sheet2!A1 = 7
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=Sheet2!A1*2");
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(14.0));
    }

    #[test]
    fn cross_sheet_reference_recalculates_when_the_other_sheet_changes() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 0), "7");
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=Sheet2!A1*2");
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(14.0));

        // Change the SOURCE on the other sheet; the dependent must follow,
        // which is the whole point of a sheet-aware dependency graph.
        w.activate(s2).unwrap();
        let rep = w.commit_edit(CellRef::new(0, 0), "10");
        assert_eq!(
            rep.recalculated, 1,
            "the Sheet1 formula is a dependent of this edit"
        );
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(20.0));
    }

    #[test]
    fn cross_sheet_range_aggregates() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        for r in 0..5u32 {
            w.commit_edit(CellRef::new(r, 0), &format!("{}", r + 1));
            w.end_edit_run();
        }
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=SUM(Sheet2!A1:A5)");
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(15.0));
        w.commit_edit(CellRef::new(1, 1), "=AVERAGE(Sheet2!A1:A5)");
        assert_eq!(val_in(&w, SheetId::MAIN, 1, 1), Value::Number(3.0));
        w.commit_edit(CellRef::new(2, 1), "=COUNT(Sheet2!A1:A5)");
        assert_eq!(val_in(&w, SheetId::MAIN, 2, 1), Value::Number(5.0));
    }

    #[test]
    fn a_cycle_spanning_two_sheets_is_detected_not_hung_on() {
        // THE acceptance criterion: Sheet1!A1 = Sheet2!A1 and
        // Sheet2!A1 = Sheet1!A1 must terminate with #CIRC!, not spin.
        let (mut w, s2) = two_sheet_wb();
        w.commit_edit(CellRef::new(0, 5), "=Sheet2!F1");
        w.activate(s2).unwrap();
        let rep = w.commit_edit(CellRef::new(0, 5), "=Sheet1!F1");
        assert!(rep.circular, "the second leg closes the loop");
        assert_eq!(
            val_in(&w, s2, 0, 5),
            Value::Error(ErrorKind::Circular),
            "a cross-sheet cycle must be flagged"
        );
        // A full recalc over the same graph must also terminate.
        w.recalc_all();
        assert_eq!(val_in(&w, s2, 0, 5), Value::Error(ErrorKind::Circular));
    }

    #[test]
    fn a_three_sheet_cycle_is_detected() {
        let (mut w, s2) = two_sheet_wb();
        let s3 = w
            .add_sheet("Sheet3", BaseData::Memory(Sheet::new("Sheet3")))
            .unwrap();
        // Sheet1!F1 -> Sheet2!F1 -> Sheet3!F1 -> Sheet1!F1
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 5), "=Sheet2!F1");
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 5), "=Sheet3!F1");
        w.activate(s3).unwrap();
        let rep = w.commit_edit(CellRef::new(0, 5), "=Sheet1!F1");
        assert!(rep.circular, "a three-sheet loop is still a loop");
    }

    #[test]
    fn a_cross_sheet_chain_that_is_not_a_cycle_evaluates_in_order() {
        // Sheet2!A1 <- Sheet1!B1 <- Sheet2!B1: the value must propagate all
        // the way through in one recalc, not settle a step at a time.
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 0), "3");
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=Sheet2!A1+1"); // 4
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=Sheet1!B1*10"); // 40
        assert_eq!(val_in(&w, s2, 0, 1), Value::Number(40.0));

        // Now change the root; both dependents must be up to date.
        w.commit_edit(CellRef::new(0, 0), "5");
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(6.0));
        assert_eq!(
            val_in(&w, s2, 0, 1),
            Value::Number(60.0),
            "the far end of the chain must see the recomputed middle"
        );
    }

    #[test]
    fn quoted_sheet_names_with_spaces_work() {
        // Documented and supported: 'My Sheet'!A1, with '' as an escaped quote.
        let mut w = wb();
        let id = w
            .add_sheet("My Sheet", BaseData::Memory(Sheet::new("My Sheet")))
            .unwrap();
        w.activate(id).unwrap();
        w.commit_edit(CellRef::new(0, 0), "12");
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 1), "='My Sheet'!A1+1");
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(13.0));
    }

    #[test]
    fn sheet_names_resolve_case_insensitively() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 0), "4");
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=sHeEt2!A1");
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(4.0));
    }

    #[test]
    fn a_reference_to_a_missing_sheet_is_a_ref_error() {
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=Nowhere!A1");
        assert_eq!(
            val_in(&w, SheetId::MAIN, 0, 1),
            Value::Error(ErrorKind::Ref),
            "a name with no sheet behind it is #REF!, not a silent zero"
        );
    }

    #[test]
    fn deleting_a_referenced_sheet_turns_dependents_into_ref_errors() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 0), "8");
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=Sheet2!A1");
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(8.0));

        w.delete_sheet(s2).unwrap();
        assert_eq!(
            val_in(&w, SheetId::MAIN, 0, 1),
            Value::Error(ErrorKind::Ref),
            "the formula must not keep showing a value from a deleted sheet"
        );
    }

    #[test]
    fn each_sheet_keeps_its_own_scroll_and_selection() {
        let (mut w, s2) = two_sheet_wb();
        // Park a distinctive position on Sheet1...
        w.set_view_state(SheetViewState {
            scroll: ScrollState {
                row_offset: 1234.0,
                col_px: 56.0,
            },
            selection: Selection::single(CellRef::new(500, 3)),
        });
        w.activate(s2).unwrap();
        // ...a fresh sheet starts at the origin, not wherever Sheet1 was.
        let fresh = w.view_state();
        assert_eq!(fresh.scroll.row_offset, 0.0);
        assert_eq!(fresh.selection.cursor, CellRef::new(0, 0));

        w.set_view_state(SheetViewState {
            scroll: ScrollState {
                row_offset: 7.0,
                col_px: 0.0,
            },
            selection: Selection::single(CellRef::new(9, 1)),
        });

        // Back to Sheet1: its own position comes back untouched.
        w.activate(SheetId::MAIN).unwrap();
        let s1 = w.view_state();
        assert_eq!(s1.scroll.row_offset, 1234.0);
        assert_eq!(s1.scroll.col_px, 56.0);
        assert_eq!(s1.selection.cursor, CellRef::new(500, 3));

        // And Sheet2 still remembers its own.
        let s2v = w.view_state_of(s2).unwrap();
        assert_eq!(s2v.scroll.row_offset, 7.0);
        assert_eq!(s2v.selection.cursor, CellRef::new(9, 1));
    }

    #[test]
    fn undo_of_an_edit_on_another_sheet_switches_back_to_it() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 0), "42");
        w.activate(SheetId::MAIN).unwrap();
        assert_eq!(w.active_sheet(), SheetId::MAIN);

        w.undo();
        assert_eq!(
            w.active_sheet(),
            s2,
            "undo must show the user the sheet it is changing"
        );
        assert_eq!(val_in(&w, s2, 0, 0), Value::Empty);
        w.redo();
        assert_eq!(val_in(&w, s2, 0, 0), Value::Number(42.0));
    }

    #[test]
    fn edits_to_the_same_cell_on_different_sheets_do_not_coalesce() {
        // The coalescing key is (sheet, cell), not just cell — otherwise a
        // quick hop between tabs would collapse two unrelated edits.
        let (mut w, s2) = two_sheet_wb();
        let before = w.undo_depth();
        w.commit_edit(CellRef::new(0, 1), "1");
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 1), "2");
        assert_eq!(w.undo_depth(), before + 2);
    }

    #[test]
    fn undo_restores_a_cross_sheet_dependent() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 0), "2");
        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=Sheet2!A1*10"); // 20
        w.activate(s2).unwrap();
        w.end_edit_run();
        w.commit_edit(CellRef::new(0, 0), "5"); // Sheet1!B1 -> 50
        assert_eq!(val_in(&w, SheetId::MAIN, 0, 1), Value::Number(50.0));

        w.undo();
        assert_eq!(val_in(&w, s2, 0, 0), Value::Number(2.0));
        assert_eq!(
            val_in(&w, SheetId::MAIN, 0, 1),
            Value::Number(20.0),
            "the dependent on the OTHER sheet must be restored too"
        );
    }

    #[test]
    fn a_formula_on_a_parked_sheet_still_recalculates() {
        // The dependent lives on a sheet the user is not looking at. It must
        // still be brought up to date — a stale value would surface the moment
        // they switched tabs.
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).unwrap();
        w.commit_edit(CellRef::new(0, 1), "=Sheet1!A1*3"); // A1 is 1 -> 3
        assert_eq!(val_in(&w, s2, 0, 1), Value::Number(3.0));

        w.activate(SheetId::MAIN).unwrap();
        w.commit_edit(CellRef::new(0, 0), "10");
        assert_eq!(
            val_in(&w, s2, 0, 1),
            Value::Number(30.0),
            "a parked sheet's formula must not go stale"
        );
    }

    #[test]
    fn single_sheet_behaviour_is_unchanged_by_the_sheet_machinery() {
        // A workbook that never grows a second sheet must behave exactly as it
        // did before: same graph keys, same recalc, same undo shape.
        let mut w = wb();
        w.commit_edit(CellRef::new(0, 1), "=A1*2");
        w.end_edit_run();
        w.commit_edit(CellRef::new(0, 2), "=B1+1");
        assert_eq!(val(&w, 0, 2), Value::Number(3.0));
        assert_eq!(w.graph.len(), 2);
        // The single-sheet convenience API still addresses these.
        assert_eq!(
            w.graph.direct_dependents(CellRef::new(0, 0)),
            vec![CellRef::new(0, 1)]
        );
        assert!(w.graph.full_order().is_ok());
    }

    // --- defined names (issue #4) ---

    /// Sheet1 with B2:B1000 filled 1..999, so a named range over it has a
    /// value worth checking rather than zero.
    fn named_wb() -> Workbook {
        let mut s = Sheet::new("Sheet1");
        for r in 1..1000u32 {
            s.set(CellRef::new(r, 1), Value::Number(r as f64));
        }
        Workbook::new(BaseData::Memory(s))
    }

    fn nsel(a: (u32, u32), b: (u32, u32)) -> Selection {
        Selection::new(CellRef::new(a.0, a.1), CellRef::new(b.0, b.1))
    }

    #[test]
    fn sum_of_a_named_range_equals_sum_of_the_explicit_range() {
        // THE acceptance criterion: =SUM(Sales) must equal
        // =SUM(Sheet1!B2:B1000), to the last float.
        let mut w = named_wb();
        w.define_name("Sales", NameScope::Workbook, nsel((1, 1), (999, 1)))
            .expect("define");

        w.commit_edit(CellRef::new(0, 4), "=SUM(Sales)");
        w.end_edit_run();
        w.commit_edit(CellRef::new(1, 4), "=SUM(Sheet1!B2:B1000)");

        let named = val(&w, 0, 4);
        let explicit = val(&w, 1, 4);
        assert_eq!(named, explicit);
        // And it is the real sum, not two matching zeros.
        assert_eq!(named, Value::Number((1..1000).sum::<u32>() as f64));
    }

    #[test]
    fn an_undefined_name_is_a_name_error_and_defining_it_repairs_the_formula() {
        let mut w = named_wb();
        w.commit_edit(CellRef::new(0, 4), "=SUM(Sales)");
        assert_eq!(
            val(&w, 0, 4),
            Value::Error(ErrorKind::Name),
            "a formula naming something undefined is #NAME?"
        );
        // Defining the name must fix it without the user retyping anything.
        w.define_name("Sales", NameScope::Workbook, nsel((1, 1), (999, 1)))
            .expect("define");
        assert_eq!(val(&w, 0, 4), Value::Number((1..1000).sum::<u32>() as f64));
    }

    #[test]
    fn renaming_a_name_rewrites_the_source_text_of_every_dependent_formula() {
        let mut w = named_wb();
        w.define_name("Sales", NameScope::Workbook, nsel((1, 1), (999, 1)))
            .expect("define");
        w.commit_edit(CellRef::new(0, 4), "=SUM(Sales)*2");
        w.end_edit_run();
        w.commit_edit(CellRef::new(1, 4), "=Sales");
        let before = val(&w, 0, 4);

        let rewritten = w
            .rename_name("Sales", &NameScope::Workbook, "Revenue")
            .expect("rename");
        assert_eq!(rewritten, 2, "both dependents rewritten");

        // The TEXT changed, not merely the resolution.
        assert_eq!(w.view().edit_text(CellRef::new(0, 4)), "=SUM(Revenue)*2");
        assert_eq!(w.view().edit_text(CellRef::new(1, 4)), "=Revenue");
        // And the value is unchanged, because the name still means the same
        // range.
        assert_eq!(val(&w, 0, 4), before);
        assert!(w.names.get("Sales", None).is_none());
        assert!(w.names.get("Revenue", None).is_some());
    }

    #[test]
    fn a_rename_preserves_absolute_markers_in_the_rewritten_formula() {
        // The reason the rewrite is textual: an AST round trip would drop
        // every `$` in the formula while renaming the name.
        let mut w = named_wb();
        w.define_name("Sales", NameScope::Workbook, nsel((1, 1), (999, 1)))
            .expect("define");
        w.commit_edit(CellRef::new(0, 4), "=SUM($B$2:$B$9)+Sales");
        w.rename_name("Sales", &NameScope::Workbook, "Revenue")
            .expect("rename");
        assert_eq!(
            w.view().edit_text(CellRef::new(0, 4)),
            "=SUM($B$2:$B$9)+Revenue",
            "the $ markers must survive a name rename"
        );
    }

    #[test]
    fn deleting_a_referenced_name_turns_its_dependents_into_name_errors() {
        let mut w = named_wb();
        w.define_name("Sales", NameScope::Workbook, nsel((1, 1), (999, 1)))
            .expect("define");
        w.commit_edit(CellRef::new(0, 4), "=SUM(Sales)");
        assert!(matches!(val(&w, 0, 4), Value::Number(_)), "healthy first");

        w.delete_name("Sales", &NameScope::Workbook)
            .expect("was defined");
        assert_eq!(val(&w, 0, 4), Value::Error(ErrorKind::Name));
        // The user's text is kept, so redefining the name restores the value.
        assert_eq!(w.view().edit_text(CellRef::new(0, 4)), "=SUM(Sales)");
    }

    #[test]
    fn a_sheet_scoped_and_a_workbook_scoped_name_resolve_per_sheet() {
        // Same identifier, two scopes: each sheet must see the right one.
        let mut s1 = Sheet::new("Sheet1");
        s1.set(CellRef::new(0, 0), Value::Number(10.0));
        let mut w = Workbook::new(BaseData::Memory(s1));

        let mut s2data = Sheet::new("Sheet2");
        s2data.set(CellRef::new(0, 3), Value::Number(99.0));
        let s2 = w
            .add_sheet("Sheet2", BaseData::Memory(s2data))
            .expect("add");

        // Workbook-scoped Total -> Sheet1!$A$1 (10).
        w.define_name_raw("Total", NameScope::Workbook, "Sheet1!$A$1")
            .expect("define workbook name");
        // Sheet-scoped Total on Sheet2 -> Sheet2!$D$1 (99).
        w.define_name_raw("Total", NameScope::Sheet("Sheet2".into()), "Sheet2!$D$1")
            .expect("define local name");

        // From Sheet1 the workbook-scoped one is the only one visible.
        w.commit_edit(CellRef::new(5, 5), "=Total");
        assert_eq!(
            val(&w, 5, 5),
            Value::Number(10.0),
            "Sheet1 must see the workbook-scoped Total"
        );

        // From Sheet2 the sheet-scoped one shadows it.
        w.activate(s2).expect("activate");
        w.commit_edit(CellRef::new(5, 5), "=Total");
        assert_eq!(
            val_in(&w, s2, 5, 5),
            Value::Number(99.0),
            "Sheet2 must see its own Total, not the workbook one"
        );

        // And Sheet1's formula is untouched by any of it.
        assert_eq!(val_in(&w, SheetId::MAIN, 5, 5), Value::Number(10.0));
    }

    #[test]
    fn the_name_box_reports_the_selection_and_navigates_to_a_name() {
        let mut w = named_wb();
        let range = nsel((1, 1), (999, 1));
        assert!(
            w.name_for_selection(range).is_none(),
            "an unnamed selection has no name"
        );
        w.define_name("Sales", NameScope::Workbook, range)
            .expect("define");
        assert_eq!(w.name_for_selection(range), Some("Sales"));
        // A different selection is still nameless.
        assert!(w.name_for_selection(nsel((0, 0), (0, 0))).is_none());

        // And the name navigates back to exactly that range.
        let (sheet, target) = w.name_target("sales").expect("resolvable");
        assert_eq!(sheet, SheetId::MAIN);
        assert_eq!(target.bounds(), range.bounds());
        assert!(w.name_target("Nope").is_none());
    }

    #[test]
    fn a_defined_name_never_materialises_the_range_it_spans() {
        // The scale invariant: a name over a huge range must produce exactly
        // one rectangular precedent, the same as the explicit range, rather
        // than an edge per row.
        let mut w = named_wb();
        w.define_name_raw("Huge", NameScope::Workbook, "Sheet1!$B$1:$B$1048576")
            .expect("define");
        w.commit_edit(CellRef::new(0, 4), "=SUM(Huge)");
        let at = SheetCell::new(SheetId::MAIN, CellRef::new(0, 4));
        let precedents = w.graph.precedents_at(at).expect("registered");
        assert_eq!(
            precedents.len(),
            1,
            "a million-row name must be ONE rectangle, not a million edges"
        );
        assert!(matches!(
            precedents[0].1,
            ferrix_formula::Precedent::Range(_, _)
        ));
    }

    #[test]
    fn names_that_would_be_ambiguous_with_a_cell_are_refused() {
        let mut w = named_wb();
        // Tax1 reads as column TAX row 1, so it could never be reached.
        assert!(matches!(
            w.define_name("Tax1", NameScope::Workbook, nsel((0, 0), (0, 0))),
            Err(NameError::LooksLikeReference(_))
        ));
        assert!(w.names.is_empty());
    }

    #[test]
    fn renaming_a_sheet_carries_its_local_names_along() {
        let mut w = named_wb();
        w.define_name_raw("Local", NameScope::Sheet("Sheet1".into()), "Sheet1!$B$2")
            .expect("define");
        w.commit_edit(CellRef::new(0, 4), "=Local");
        let before = val(&w, 0, 4);
        assert_eq!(before, Value::Number(1.0), "B2 holds 1");

        w.rename_sheet(SheetId::MAIN, "Revenue Q1").expect("rename");
        assert_eq!(
            val(&w, 0, 4),
            before,
            "a local name must follow its sheet's rename, not break"
        );
        assert_eq!(
            w.names.get("Local", Some("Revenue Q1")).unwrap().refers_to,
            "'Revenue Q1'!$B$2"
        );
    }

    #[test]
    fn deleting_a_sheet_drops_its_local_names() {
        let (mut w, s2) = two_sheet_wb();
        w.activate(s2).expect("activate");
        w.define_name_raw("Local", NameScope::Sheet("Sheet2".into()), "Sheet2!$A$1")
            .expect("define");
        w.activate(SheetId::MAIN).expect("activate");
        w.delete_sheet(s2).expect("delete");
        assert!(
            w.names.is_empty(),
            "a name scoped to a deleted sheet must not outlive it"
        );
    }

    #[test]
    fn a_duplicate_name_in_the_same_scope_is_refused() {
        let mut w = named_wb();
        let r = nsel((1, 1), (999, 1));
        w.define_name("Sales", NameScope::Workbook, r)
            .expect("first");
        assert!(matches!(
            w.define_name("SALES", NameScope::Workbook, r),
            Err(NameError::Duplicate(_))
        ));
        assert_eq!(w.names.len(), 1);
    }

    // --- Goal Seek (issue #35) -------------------------------------------
    //
    // What each of these would say if `goal_seek` did nothing at all: every
    // one asserts on a NUMBER the solver had to compute, or on an undo depth
    // that only a committed edit can produce. A no-op solver fails all of
    // them.

    #[test]
    fn goal_seek_hits_a_linear_target() {
        let mut w = wb();
        let b = CellRef::new(0, 1); // B1, the changing cell
        let a = CellRef::new(0, 2); // C1 = B1*3 + 4, the target
        w.commit_edit(b, "1");
        w.commit_edit(a, "=B1*3+4");
        assert_eq!(val(&w, 0, 2), Value::Number(7.0), "setup");

        let rep = w.goal_seek(a, 25.0, b).expect("A depends on B");

        assert!(rep.converged, "a linear model must converge: {rep:?}");
        // B must be 7 (7*3+4 = 25), not merely "some number".
        assert!(
            (rep.final_b - 7.0).abs() < 1e-6,
            "changing cell should land on 7, got {}",
            rep.final_b
        );
        assert!((rep.final_a.expect("numeric") - 25.0).abs() < GOAL_SEEK_EPSILON);
        // And the SHEET, not just the report, must hold the answer.
        let on_sheet = val(&w, 0, 1).as_number().expect("B1 is a number");
        assert!((on_sheet - 7.0).abs() < 1e-6, "sheet holds {on_sheet}");
        assert!((val(&w, 0, 2).as_number().unwrap() - 25.0).abs() < GOAL_SEEK_EPSILON);
    }

    #[test]
    fn goal_seek_refuses_when_the_target_does_not_depend_on_the_changing_cell() {
        let mut w = wb();
        let b = CellRef::new(0, 1);
        let a = CellRef::new(0, 2);
        w.commit_edit(b, "1");
        w.commit_edit(a, "=A1*3"); // reads A1, NOT B1
        let before_depth = w.undo_depth();
        let before_a = val(&w, 0, 2);

        let err = w.goal_seek(a, 999.0, b).expect_err("must refuse");
        assert_eq!(err, GoalSeekError::NotDependent);
        // Refusal is total: no edit, no history, no recalculated value.
        assert_eq!(val(&w, 0, 1), Value::Number(1.0), "B1 untouched");
        assert_eq!(val(&w, 0, 2), before_a, "the target never moved");
        assert_eq!(w.undo_depth(), before_depth, "no undo entry was pushed");
    }

    #[test]
    fn goal_seek_works_several_hops_downstream() {
        // E1 <- D1 <- C1 <- B1. E1 never mentions B1, so this only works if
        // the dependency check and the recalculation both go transitive.
        let mut w = wb();
        let b = CellRef::new(0, 1);
        let e = CellRef::new(0, 4);
        w.commit_edit(b, "2");
        w.commit_edit(CellRef::new(0, 2), "=B1*2"); // C1
        w.commit_edit(CellRef::new(0, 3), "=C1+10"); // D1
        w.commit_edit(e, "=D1*5"); // E1 = ((B1*2)+10)*5
        assert_eq!(val(&w, 0, 4), Value::Number(70.0), "setup");

        // Want E1 = 200 => D1 = 40 => C1 = 30 => B1 = 15.
        let rep = w.goal_seek(e, 200.0, b).expect("E1 depends on B1");
        assert!(rep.converged, "{rep:?}");
        assert!(
            (rep.final_b - 15.0).abs() < 1e-6,
            "B1 should be 15, got {}",
            rep.final_b
        );
        assert!((val(&w, 0, 4).as_number().unwrap() - 200.0).abs() < GOAL_SEEK_EPSILON);
        // The intermediate cells must have been recalculated too, not left
        // stale at their setup values.
        assert!((val(&w, 0, 2).as_number().unwrap() - 30.0).abs() < 1e-6);
        assert!((val(&w, 0, 3).as_number().unwrap() - 40.0).abs() < 1e-6);
    }

    #[test]
    fn goal_seek_is_exactly_one_undo_step_and_undo_restores_b() {
        let mut w = wb();
        let b = CellRef::new(0, 1);
        let a = CellRef::new(0, 2);
        w.commit_edit(b, "1");
        w.commit_edit(a, "=B1*3+4");
        w.end_edit_run();
        let depth_before = w.undo_depth();

        let rep = w.goal_seek(a, 25.0, b).expect("depends");
        assert!(rep.converged);
        // The whole run — however many probes it took — is ONE entry.
        assert_eq!(
            w.undo_depth(),
            depth_before + 1,
            "goal seek ran {} iterations and must still be a single undo step",
            rep.iterations
        );
        assert!(rep.iterations > 1, "setup: the search really did iterate");

        w.undo();
        // One undo puts B1 AND the dependent target back where they started.
        assert_eq!(val(&w, 0, 1), Value::Number(1.0), "B1 restored by one undo");
        assert_eq!(val(&w, 0, 2), Value::Number(7.0), "target restored too");
        assert_eq!(w.undo_depth(), depth_before, "no leftover probe entries");
    }

    #[test]
    fn goal_seek_reports_the_closest_value_rather_than_claiming_success() {
        // C1 = B1*B1 + 5 has a minimum of 5, so a target of 1 is unreachable.
        let mut w = wb();
        let b = CellRef::new(0, 1);
        let c = CellRef::new(0, 2);
        w.commit_edit(b, "3");
        w.commit_edit(c, "=B1*B1+5");

        let rep = w.goal_seek(c, 1.0, b).expect("C1 depends on B1");
        assert!(
            !rep.converged,
            "an unreachable target must NOT be reported as converged: {rep:?}"
        );
        let got = rep.final_a.expect("a number was reached");
        // The honest answer is the closest approach it actually reached. The
        // true infimum is 5 (at B1 = 0) and is approached asymptotically, so
        // pin the properties that matter rather than an exact landing point:
        //  * it never claims to have reached 1;
        //  * it cannot report a value BELOW 5, which the model cannot produce;
        //  * it is much closer than where it started (B1 = 3 gives 14).
        assert!(
            got >= 5.0,
            "reported {got}, which C1 = B1*B1+5 can never produce"
        );
        assert!(
            (got - 1.0).abs() > 1.0,
            "must not pretend it reached the target: {got}"
        );
        let start_err = (14.0f64 - 1.0).abs();
        assert!(
            (got - 1.0).abs() < start_err,
            "the closest approach ({got}) must beat the starting value (14)"
        );
        assert!(
            (got - 5.0).abs() < 0.5,
            "should get near the reachable minimum of 5, got {got}"
        );
        assert!(
            rep.final_b.abs() < 0.5,
            "closest approach is near B1 = 0, got {}",
            rep.final_b
        );
        // The sheet must agree with the report.
        assert!((val(&w, 0, 2).as_number().unwrap() - got).abs() < 1e-6);
        assert!((val(&w, 0, 1).as_number().unwrap() - rep.final_b).abs() < 1e-9);
    }

    #[test]
    fn goal_seek_terminates_on_a_divergent_target() {
        // C1 = 1/B1 + 1000 can never reach 0: as the secant pushes B1 away
        // the value tends to 1000 and the step diverges. This must stop, not
        // spin, and must stop at or under the iteration cap.
        let mut w = wb();
        let b = CellRef::new(0, 1);
        let c = CellRef::new(0, 2);
        w.commit_edit(b, "1");
        w.commit_edit(c, "=1/B1+1000");

        let start = std::time::Instant::now();
        let rep = w.goal_seek(c, 0.0, b).expect("C1 depends on B1");
        let elapsed = start.elapsed();

        assert!(!rep.converged, "0 is unreachable: {rep:?}");
        assert!(
            rep.iterations <= GOAL_SEEK_MAX_ITERS,
            "the cap must hold: {} iterations",
            rep.iterations
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "a divergent search must terminate promptly, took {elapsed:?}"
        );
        // Whatever it reports must be a real, finite number the user can read.
        assert!(rep.final_b.is_finite(), "final_b = {}", rep.final_b);
        assert!(rep.final_a.expect("numeric").is_finite());
    }

    #[test]
    fn goal_seek_refuses_to_overwrite_a_formula_in_the_changing_cell() {
        let mut w = wb();
        let b = CellRef::new(0, 1);
        let c = CellRef::new(0, 2);
        w.commit_edit(b, "=A1+1"); // B1 is COMPUTED
        w.commit_edit(c, "=B1*3");
        let depth = w.undo_depth();

        let err = w.goal_seek(c, 100.0, b).expect_err("must refuse");
        assert_eq!(err, GoalSeekError::ChangingCellIsFormula);
        // The formula must still be there, character for character.
        assert_eq!(
            w.view().edit_text(b),
            "=A1+1",
            "the changing cell's formula must survive the refusal"
        );
        assert_eq!(w.undo_depth(), depth);
    }

    #[test]
    fn goal_seek_starting_from_zero_still_finds_a_second_sample() {
        // A secant needs two distinct x values. Starting at exactly 0 with a
        // purely proportional step would give 0 twice and divide by zero.
        let mut w = wb();
        let b = CellRef::new(0, 1);
        let c = CellRef::new(0, 2);
        w.commit_edit(b, "0");
        w.commit_edit(c, "=B1*4");

        let rep = w.goal_seek(c, 12.0, b).expect("depends");
        assert!(rep.converged, "{rep:?}");
        assert!(
            (rep.final_b - 3.0).abs() < 1e-6,
            "B1 should be 3, got {}",
            rep.final_b
        );
    }

    #[test]
    fn goal_seek_sees_through_a_range_precedent() {
        // C1 = SUM(A1:A10); A3 is inside that range, so changing A3 moves C1
        // even though C1 names no individual cell.
        let mut w = wb();
        let a3 = CellRef::new(2, 0); // base value 3
        let c = CellRef::new(0, 2);
        w.commit_edit(c, "=SUM(A1:A10)");
        assert_eq!(val(&w, 0, 2), Value::Number(55.0), "setup");

        let rep = w.goal_seek(c, 100.0, a3).expect("C1 depends on A3");
        assert!(rep.converged, "{rep:?}");
        // 55 - 3 + x = 100 => x = 48.
        assert!(
            (rep.final_b - 48.0).abs() < 1e-6,
            "A3 should be 48, got {}",
            rep.final_b
        );
        assert!((val(&w, 0, 2).as_number().unwrap() - 100.0).abs() < GOAL_SEEK_EPSILON);
    }

    #[test]
    fn a_failed_goal_seek_leaves_the_sheet_exactly_as_it_was() {
        // The refusal paths are cheap, but the SEARCH path also has to be
        // clean when it never finds a number: no probe value may survive.
        let mut w = wb();
        let b = CellRef::new(0, 1);
        let c = CellRef::new(0, 2);
        w.commit_edit(b, "5");
        w.commit_edit(c, "=B1&\"x\""); // text: never a number
        let depth = w.undo_depth();

        let rep = w.goal_seek(c, 10.0, b).expect("C1 does depend on B1");
        assert!(!rep.converged);
        assert_eq!(rep.final_a, None, "the target never produced a number");
        assert_eq!(
            val(&w, 0, 1),
            Value::Number(5.0),
            "B1 must be back at its original value, not at a leftover probe"
        );
        assert_eq!(w.undo_depth(), depth, "nothing to undo when nothing landed");
    }
}
