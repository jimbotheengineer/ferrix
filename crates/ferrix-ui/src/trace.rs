//! Trace Precedents / Trace Dependents (roadmap #39).
//!
//! Pure graph-walk logic, kept free of egui so it can be unit tested without
//! a harness frame. `app.rs` owns painting the arrows this module computes.

use ferrix_core::{CellRef, SheetCell};
use ferrix_formula::depgraph::DepGraph;

/// Which direction the current trace is walking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceKind {
    Precedents,
    Dependents,
}

/// An active trace session: the cell it started from, which direction, and
/// how many levels out repeated invocations have walked.
///
/// Excel's behaviour is what `depth` reproduces: pressing Trace Precedents
/// again on the same cell walks one level further out rather than resetting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceState {
    pub origin: SheetCell,
    pub kind: TraceKind,
    pub depth: usize,
}

impl TraceState {
    pub fn new(origin: SheetCell, kind: TraceKind) -> Self {
        Self {
            origin,
            kind,
            depth: 1,
        }
    }
}

/// One arrow: from `from` to `to`, both workbook-wide addresses so a trace
/// that crosses sheets is representable even though only the active sheet's
/// endpoints are ever painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: SheetCell,
    pub to: SheetCell,
}

/// Hard cap on arrows drawn in one frame. A cell with 500k dependents must
/// not attempt 500k arrows — see AGENT_GUIDE.md's scale invariant.
pub const MAX_ARROWS: usize = 100;

/// Every edge the current trace level covers, and how many there would be
/// with no cap — the second number is what the "showing N of M" note reports.
///
/// BFS outward from `origin`, `depth` levels deep. Each level's cells become
/// the next level's frontier, exactly as repeated Trace invocations walk
/// further out in Excel. A cell is only ever visited once (`seen`), so a
/// diamond-shaped dependency graph does not revisit the same cell and does
/// not loop forever on a cycle.
pub fn edges_for(graph: &DepGraph, state: TraceState) -> (Vec<Edge>, usize) {
    let mut all: Vec<Edge> = Vec::new();
    let mut frontier = vec![state.origin];
    let mut seen = std::collections::HashSet::new();
    seen.insert(state.origin);

    for _ in 0..state.depth {
        let mut next = Vec::new();
        for &cell in &frontier {
            match state.kind {
                TraceKind::Precedents => {
                    if let Some(prec) = graph.precedents_at(cell) {
                        for &(sheet, p) in prec {
                            for target in precedent_cells(p) {
                                let to = SheetCell::new(sheet, target);
                                all.push(Edge { from: cell, to });
                                if seen.insert(to) {
                                    next.push(to);
                                }
                            }
                        }
                    }
                }
                TraceKind::Dependents => {
                    for dep in graph.direct_dependents_at(cell) {
                        all.push(Edge {
                            from: cell,
                            to: dep,
                        });
                        if seen.insert(dep) {
                            next.push(dep);
                        }
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    // Deterministic order: painting (and tests) should not depend on hash
    // iteration order.
    all.sort_by_key(|e| (e.from, e.to));
    all.dedup();
    let total = all.len();
    all.truncate(MAX_ARROWS);
    (all, total)
}

/// A precedent's individual cells. A range precedent expands into its
/// corners only for arrow-drawing purposes (the endpoints are what an arrow
/// needs) — NOT into every contained cell, which would defeat the whole
/// point of the graph storing rectangles instead of edges.
fn precedent_cells(p: ferrix_formula::depgraph::Precedent) -> Vec<CellRef> {
    use ferrix_formula::depgraph::Precedent;
    match p {
        Precedent::Cell(c) => vec![c],
        // A range arrow points at its top-left corner, matching how Excel
        // draws one arrowhead into a referenced range rather than one per
        // cell in it.
        Precedent::Range(a, _) => vec![a],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_formula::parser::parse;

    fn sc(row: u32, col: u32) -> SheetCell {
        SheetCell::new(ferrix_core::SheetId::MAIN, CellRef::new(row, col))
    }

    fn graph_with(formulas: &[(SheetCell, &str)]) -> DepGraph {
        let mut g = DepGraph::new();
        for &(at, src) in formulas {
            let expr = parse(src).unwrap();
            g.set_formula_at(at, &expr, &ferrix_formula::depgraph::SheetIndex::default());
        }
        g
    }

    #[test]
    fn what_would_this_report_if_tracing_did_nothing() {
        // The AGENT_GUIDE question, applied directly: an empty graph must
        // yield zero edges, not some default/placeholder arrow.
        let g = DepGraph::new();
        let state = TraceState::new(sc(0, 0), TraceKind::Precedents);
        let (edges, total) = edges_for(&g, state);
        assert!(edges.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn precedents_of_a_simple_formula() {
        // B1 = A1 + C1
        let g = graph_with(&[(sc(0, 1), "=A1+C1")]);
        let state = TraceState::new(sc(0, 1), TraceKind::Precedents);
        let (edges, total) = edges_for(&g, state);
        assert_eq!(total, 2);
        assert!(edges.contains(&Edge {
            from: sc(0, 1),
            to: sc(0, 0)
        }));
        assert!(edges.contains(&Edge {
            from: sc(0, 1),
            to: sc(0, 2)
        }));
    }

    #[test]
    fn dependents_of_a_source_cell() {
        // B1 = A1*2, C1 = A1+1 — both directly depend on A1.
        let g = graph_with(&[(sc(0, 1), "=A1*2"), (sc(0, 2), "=A1+1")]);
        let state = TraceState::new(sc(0, 0), TraceKind::Dependents);
        let (edges, total) = edges_for(&g, state);
        assert_eq!(total, 2);
        assert!(edges.iter().any(|e| e.to == sc(0, 1)));
        assert!(edges.iter().any(|e| e.to == sc(0, 2)));
    }

    #[test]
    fn repeated_invocations_walk_one_level_further_out() {
        // A1 <- B1 <- C1 (C1 depends on B1, B1 depends on A1). Precedents of
        // C1 at depth 1 reach only B1; at depth 2 they also reach A1.
        let g = graph_with(&[(sc(0, 1), "=A1"), (sc(0, 2), "=B1")]);
        let mut state = TraceState::new(sc(0, 2), TraceKind::Precedents);
        let (edges1, _) = edges_for(&g, state);
        assert_eq!(
            edges1,
            vec![Edge {
                from: sc(0, 2),
                to: sc(0, 1)
            }]
        );

        state.depth = 2;
        let (edges2, _) = edges_for(&g, state);
        assert_eq!(edges2.len(), 2, "depth 2 must also reach A1 via B1");
        assert!(edges2.contains(&Edge {
            from: sc(0, 1),
            to: sc(0, 0)
        }));
    }

    #[test]
    fn a_cell_with_500k_dependents_is_capped_not_attempted_in_full() {
        // The scale invariant, exercised directly: build one source cell
        // with far more dependents than the cap, and assert the arrow list
        // never exceeds MAX_ARROWS while `total` still reports the truth.
        let mut formulas: Vec<(SheetCell, String)> = Vec::new();
        for i in 0..(MAX_ARROWS * 5) as u32 {
            formulas.push((sc(i + 1, 0), "=A1".to_string()));
        }
        let refs: Vec<(SheetCell, &str)> = formulas.iter().map(|(c, s)| (*c, s.as_str())).collect();
        let g = graph_with(&refs);
        let state = TraceState::new(sc(0, 0), TraceKind::Dependents);
        let (edges, total) = edges_for(&g, state);
        assert_eq!(total, MAX_ARROWS * 5);
        assert_eq!(
            edges.len(),
            MAX_ARROWS,
            "arrows must be capped at MAX_ARROWS"
        );
    }

    #[test]
    fn a_cycle_terminates_instead_of_looping_forever() {
        // A1 = B1, B1 = A1 — a direct two-cell cycle. Depth 3 must still
        // return promptly rather than spinning; `seen` is what guarantees
        // it never keeps re-expanding the same cell's frontier forever.
        // Both real edges (A1->B1 and B1->A1) are legitimately found —
        // walking the cycle once more does not fabricate a THIRD edge,
        // which is the property that actually distinguishes termination
        // from an infinite loop that happens to get interrupted.
        let g = graph_with(&[(sc(0, 0), "=B1"), (sc(0, 1), "=A1")]);
        let mut state = TraceState::new(sc(0, 0), TraceKind::Precedents);
        state.depth = 3;
        let (edges, total) = edges_for(&g, state);
        assert_eq!(
            total, 2,
            "the cycle has exactly two real edges, not three or more"
        );
        assert!(edges.contains(&Edge {
            from: sc(0, 0),
            to: sc(0, 1)
        }));
        assert!(edges.contains(&Edge {
            from: sc(0, 1),
            to: sc(0, 0)
        }));
    }
}
