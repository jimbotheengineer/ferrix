//! Workbook: edit application, recalculation, and undo/redo.
//!
//! This is where a keystroke becomes a committed, recalculated change. It owns
//! the overlay and the dependency graph and keeps them consistent, so the UI
//! only has to say "the user typed X into A1".

use ferrix_core::{CellInput, CellRef, EditOverlay, ErrorKind, Value};
use ferrix_formula::depgraph::DepGraph;
use ferrix_formula::{eval_view, parse};

use crate::sheet_view::{BaseData, SheetView};

/// One undoable action.
#[derive(Debug)]
pub struct UndoEntry {
    cell: CellRef,
    before: Option<CellInput>,
    after: Option<CellInput>,
    /// Formula cells whose cached values changed as a side effect, so undo
    /// restores the whole visible state rather than leaving stale results.
    side_effects: Vec<(CellRef, Value)>,
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
}

impl Workbook {
    pub fn new(base: BaseData) -> Self {
        Self {
            base,
            overlay: EditOverlay::new(),
            graph: DepGraph::new(),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn view(&self) -> SheetView<'_> {
        SheetView::new(&self.base, &self.overlay)
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
        self.undo.push(UndoEntry {
            cell,
            before,
            after,
            side_effects,
        });
        // A fresh edit invalidates the redo branch.
        self.redo.clear();

        report.micros = start.elapsed().as_micros();
        report
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
        self.overlay.restore(entry.cell, entry.before.clone());
        // Restore dependent caches captured at commit time.
        for (dep, prev) in &entry.side_effects {
            self.overlay.update_cached(*dep, *prev);
        }
        self.resync_graph(entry.cell);
        let cell = entry.cell;
        self.redo.push(entry);
        Some(cell)
    }

    pub fn redo(&mut self) -> Option<CellRef> {
        let entry = self.redo.pop()?;
        self.overlay.restore(entry.cell, entry.after.clone());
        self.resync_graph(entry.cell);
        let cell = entry.cell;
        // Re-derive dependents rather than trusting stale caches.
        if let Ok(order) = self.graph.recalc_order(cell) {
            for dep in order {
                self.eval_one(dep);
            }
        }
        self.undo.push(entry);
        Some(cell)
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
        w.commit_edit(c, "1");
        w.commit_edit(c, "2");
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
}
