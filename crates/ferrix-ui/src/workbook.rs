//! Workbook: edit application, recalculation, and undo/redo.
//!
//! This is where a keystroke becomes a committed, recalculated change. It owns
//! the overlay and the dependency graph and keeps them consistent, so the UI
//! only has to say "the user typed X into A1".

use ferrix_core::{CellInput, CellRef, EditOverlay, ErrorKind, Selection, Value};
use ferrix_formula::depgraph::DepGraph;
use ferrix_formula::fill::FillKind;
use ferrix_formula::{eval_view, parse};

use crate::sheet_view::{BaseData, SheetView};

/// One undoable action.
///
/// `changes` holds every cell the action touched, so a bulk operation (paste,
/// clearing a selected range) is a single undo step rather than one per cell.
/// A plain single-cell edit is just a one-element batch.
#[derive(Debug)]
pub struct UndoEntry {
    /// Where to put the cursor when this entry is undone or redone.
    cell: CellRef,
    changes: Vec<CellChange>,
    /// Formula cells whose cached values changed as a side effect, so undo
    /// restores the whole visible state rather than leaving stale results.
    side_effects: Vec<(CellRef, Value)>,
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

pub struct Workbook {
    pub base: BaseData,
    pub overlay: EditOverlay,
    pub graph: DepGraph,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    /// Maximum number of undo entries kept. When exceeded, the OLDEST entry is
    /// dropped — recent history is what users actually reach for.
    undo_limit: usize,
    /// The cell and wall-clock time of the last single-cell edit, used to
    /// decide whether the next edit folds into it. Cleared by anything that
    /// breaks the "same continuous edit" story: a bulk op, an undo, a redo, a
    /// save, or an edit to a different cell.
    last_edit: Option<(CellRef, std::time::Instant)>,
    /// Edits made since the last save. Drives the dirty indicator and the
    /// close prompt; without it a user can lose work by closing the window.
    dirty: bool,
}

impl Workbook {
    pub fn new(base: BaseData) -> Self {
        Self {
            base,
            overlay: EditOverlay::new(),
            graph: DepGraph::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            undo_limit: DEFAULT_UNDO_LIMIT,
            last_edit: None,
            dirty: false,
        }
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
        let formulas: Vec<(CellRef, String)> = self
            .overlay
            .formula_cells()
            .map(|(c, s)| (c, s.to_string()))
            .collect();
        for (cell, src) in &formulas {
            if let Ok(expr) = parse(src) {
                self.graph.set_formula(*cell, &expr);
            }
        }
        // Evaluate in dependency order so a formula referencing another
        // formula sees an up-to-date value rather than a stale cache.
        // Cycles come back as Err; those cells get #CIRC! rather than a stale
        // cached value silently surviving from the saved file.
        let (order, circular) = match self.graph.full_order() {
            Ok(o) => (o, Vec::new()),
            Err(stuck) => (Vec::new(), stuck),
        };
        for cell in circular {
            self.overlay
                .update_cached(cell, Value::Error(ErrorKind::Circular));
        }
        for cell in order {
            let Some(src) = self
                .overlay
                .get(cell)
                .and_then(|i| i.formula_src())
                .map(|s| s.to_string())
            else {
                continue;
            };
            if let Ok(expr) = parse(&src) {
                let value = {
                    let view = SheetView::new(&self.base, &self.overlay);
                    eval_view(&expr, &view)
                };
                self.overlay.update_cached(cell, value);
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

    /// Commit what the user typed into `cell`, then recalculate dependents.
    pub fn commit_edit(&mut self, cell: CellRef, raw: &str) -> CommitReport {
        let start = std::time::Instant::now();
        let mut report = CommitReport::default();
        self.dirty = true;

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
                    self.graph.set_formula(cell, &expr);
                    if self.graph.is_circular(cell) {
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
                    self.graph.remove(cell);
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
                self.graph.remove(cell);
            }
        }

        // Evaluate this cell if it is a healthy formula.
        if !report.circular && report.parse_error.is_none() {
            self.eval_one(cell);
        }

        // Recalculate everything downstream, in dependency order.
        let mut side_effects = Vec::new();
        match self.graph.recalc_order(cell) {
            Ok(order) => {
                for dep in order {
                    let prev = self.overlay.value(dep).unwrap_or(Value::Empty);
                    self.eval_one(dep);
                    let now = self.overlay.value(dep).unwrap_or(Value::Empty);
                    if prev != now {
                        side_effects.push((dep, prev));
                    }
                    report.recalculated += 1;
                }
            }
            Err(cycle) => {
                report.circular = true;
                for c in cycle {
                    self.overlay
                        .update_cached(c, Value::Error(ErrorKind::Circular));
                }
            }
        }

        let after = self.overlay.get(cell).cloned();
        let now = std::time::Instant::now();

        // Coalesce: if the previous undo entry is a single-cell edit to THIS
        // same cell, made within COALESCE_WINDOW, fold this edit into it so one
        // logical edit is one undo step. Keep the older `before` (undo must
        // rewind to the state before the burst started) and adopt the new
        // `after`.
        let coalesce = match (self.last_edit, self.undo.last()) {
            (Some((last_cell, at)), Some(top)) => {
                last_cell == cell
                    && !top.bulk
                    && top.changes.len() == 1
                    && top.changes[0].cell == cell
                    && now.duration_since(at) < COALESCE_WINDOW
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
        self.last_edit = Some((cell, now));

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

    /// Evaluate a single formula cell and store its result.
    fn eval_one(&mut self, cell: CellRef) {
        let src = match self.overlay.get(cell) {
            Some(CellInput::Formula { src, .. }) => src.clone(),
            _ => return,
        };
        let value = match parse(&src) {
            Ok(expr) => {
                let view = SheetView::new(&self.base, &self.overlay);
                eval_view(&expr, &view)
            }
            Err(_) => Value::Error(ErrorKind::Name),
        };
        self.overlay.update_cached(cell, value);
    }

    /// Recompute every formula in dependency order — used after a bulk change.
    pub fn recalc_all(&mut self) -> usize {
        match self.graph.full_order() {
            Ok(order) => {
                let n = order.len();
                for cell in order {
                    self.eval_one(cell);
                }
                n
            }
            Err(cycle) => {
                let n = cycle.len();
                for c in cycle {
                    self.overlay
                        .update_cached(c, Value::Error(ErrorKind::Circular));
                }
                n
            }
        }
    }

    pub fn undo(&mut self) -> Option<CellRef> {
        let entry = self.undo.pop()?;
        self.dirty = true;
        // An undo ends any coalescing run: the next keystroke must not fold
        // into an entry the user has just stepped away from.
        self.last_edit = None;
        // Reverse order so overlapping writes unwind exactly as they were made.
        for ch in entry.changes.iter().rev() {
            self.overlay.restore(ch.cell, ch.before.clone());
            self.resync_graph(ch.cell);
        }
        // Restore dependent caches captured at commit time.
        for (dep, prev) in &entry.side_effects {
            self.overlay.update_cached(*dep, *prev);
        }
        let cell = entry.cell;
        self.redo.push(entry);
        Some(cell)
    }

    pub fn redo(&mut self) -> Option<CellRef> {
        let entry = self.redo.pop()?;
        self.dirty = true;
        self.last_edit = None;
        for ch in &entry.changes {
            self.overlay.restore(ch.cell, ch.after.clone());
            self.resync_graph(ch.cell);
        }
        // Re-derive dependents rather than trusting stale caches.
        let touched: Vec<CellRef> = entry.changes.iter().map(|c| c.cell).collect();
        for cell in touched {
            if let Ok(order) = self.graph.recalc_order(cell) {
                for dep in order {
                    self.eval_one(dep);
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
            self.graph.remove(cell);
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
        match self.overlay.get(cell) {
            Some(CellInput::Formula { src, .. }) => {
                let src = src.clone();
                match parse(&src) {
                    Ok(expr) => self.graph.set_formula(cell, &expr),
                    Err(_) => self.graph.remove(cell),
                }
            }
            _ => self.graph.remove(cell),
        }
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
}
