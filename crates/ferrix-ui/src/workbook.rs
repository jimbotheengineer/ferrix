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
use ferrix_formula::{eval_view, parse};

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
}

/// Why a sheet operation was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum SheetError {
    DuplicateName(String),
    EmptyName,
    LastSheet,
    NoSuchSheet,
}

impl std::fmt::Display for SheetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SheetError::DuplicateName(n) => write!(f, "a sheet named {n:?} already exists"),
            SheetError::EmptyName => write!(f, "a sheet name cannot be blank"),
            SheetError::LastSheet => write!(f, "a workbook must keep at least one sheet"),
            SheetError::NoSuchSheet => write!(f, "no such sheet"),
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
    /// Tab order and per-sheet identity/view state. Never empty.
    sheets: Vec<SheetMeta>,
    /// Index into `sheets` of the sheet whose data is in `base`/`overlay`.
    active: usize,
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
    /// Edits made since the last save. Drives the dirty indicator and the
    /// close prompt; without it a user can lose work by closing the window.
    dirty: bool,
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
            sheets: vec![SheetMeta {
                // The first sheet is always MAIN, which is what makes a
                // single-sheet workbook's graph identical to the pre-sheets one.
                id: SheetId::MAIN,
                name: name.to_string(),
                view: SheetViewState::default(),
            }],
            active: 0,
            parked: std::collections::HashMap::new(),
            next_id: 1,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_limit: DEFAULT_UNDO_LIMIT,
            last_edit: None,
            dirty: false,
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
        let name = self.validate_name(name, Some(id))?;
        let idx = self.index_of(id).ok_or(SheetError::NoSuchSheet)?;
        self.sheets[idx].name = name;
        self.dirty = true;
        self.rebuild_graph_and_recalc();
        Ok(())
    }

    /// Delete a sheet and everything it stored. The last sheet cannot go.
    pub fn delete_sheet(&mut self, id: SheetId) -> Result<(), SheetError> {
        if self.sheets.len() == 1 {
            return Err(SheetError::LastSheet);
        }
        let idx = self.index_of(id).ok_or(SheetError::NoSuchSheet)?;
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
        self.dirty = true;
        self.last_edit = None;
        self.rebuild_graph_and_recalc();
        Ok(())
    }

    /// Move a sheet to a new position in the tab strip.
    pub fn reorder_sheet(&mut self, id: SheetId, to: usize) -> Result<(), SheetError> {
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

    // --------------------------------------------------------------- editing

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
            if let Ok(expr) = parse(src) {
                self.graph.set_formula_at(*at, &expr, &resolve);
            }
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
        SheetView::new(&self.base, &self.overlay)
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
    pub fn commit_edit(&mut self, cell: CellRef, raw: &str) -> CommitReport {
        let start = std::time::Instant::now();
        let mut report = CommitReport::default();
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
            Some(CellInput::Formula { src, .. }) => match parse(src) {
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
            },
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
        let value = match parse(&src) {
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

    /// Clear every cell in a selection as ONE undo step.
    pub fn clear_range(&mut self, sel: Selection, max_cells: u64) -> Result<usize, String> {
        if sel.cell_count() > max_cells {
            return Err(format!(
                "{} cells is too many to clear at once (limit {})",
                sel.cell_count(),
                max_cells
            ));
        }
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

    /// Paste a block of text with its top-left corner at `origin`, as ONE
    /// undo step. Returns how many cells were written.
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
            Some(src) => match parse(&src) {
                Ok(expr) => {
                    // Resolve names against the CURRENT sheet list, so a
                    // reference to a sheet that no longer exists drops out of
                    // the graph instead of dangling.
                    let resolve = self.name_resolver();
                    self.graph.set_formula_at(at, &expr, &resolve);
                }
                Err(_) => self.graph.remove_at(at),
            },
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
}
