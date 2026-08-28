//! Dependency graph and incremental recalculation.
//!
//! When a cell changes, only its dependents need recomputing — and they must
//! be recomputed in dependency order, or a formula will read a stale input.
//!
//! Design at scale: the graph holds only *formula* cells and their references.
//! A 200M-row file with 50 formulas is a 50-node graph. Data cells never enter
//! it; they are leaves that formulas point at.
//!
//! Range dependencies (`SUM(A1:A10000000)`) are stored as rectangles, not
//! expanded into ten million edges. Checking whether a changed cell affects a
//! formula is then a rectangle containment test.

use std::collections::{HashMap, HashSet, VecDeque};

use ferrix_core::CellRef;

use crate::parser::Expr;

/// A precedent: what a formula reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Precedent {
    Cell(CellRef),
    /// Inclusive rectangle, normalized so start <= end on both axes.
    Range(CellRef, CellRef),
}

impl Precedent {
    /// Does this precedent include `cell`?
    #[inline]
    pub fn contains(&self, cell: CellRef) -> bool {
        match self {
            Precedent::Cell(c) => *c == cell,
            Precedent::Range(a, b) => {
                cell.row >= a.row && cell.row <= b.row && cell.col >= a.col && cell.col <= b.col
            }
        }
    }
}

/// Walk an expression and collect everything it reads.
pub fn collect_precedents(expr: &Expr, out: &mut Vec<Precedent>) {
    match expr {
        Expr::Ref(c) => out.push(Precedent::Cell(*c)),
        Expr::Range(a, b) => out.push(Precedent::Range(*a, *b)),
        Expr::Unary(_, inner) => collect_precedents(inner, out),
        Expr::Binary(_, l, r) => {
            collect_precedents(l, out);
            collect_precedents(r, out);
        }
        Expr::Call(_, args) => {
            for a in args {
                collect_precedents(a, out);
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) => {}
    }
}

/// Formula dependency graph.
#[derive(Debug, Default)]
pub struct DepGraph {
    /// formula cell -> what it reads
    precedents: HashMap<CellRef, Vec<Precedent>>,
}

impl DepGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.precedents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.precedents.is_empty()
    }

    /// Register (or replace) a formula's dependencies.
    pub fn set_formula(&mut self, cell: CellRef, expr: &Expr) {
        let mut p = Vec::new();
        collect_precedents(expr, &mut p);
        self.precedents.insert(cell, p);
    }

    pub fn remove(&mut self, cell: CellRef) {
        self.precedents.remove(&cell);
    }

    pub fn precedents_of(&self, cell: CellRef) -> Option<&[Precedent]> {
        self.precedents.get(&cell).map(|v| v.as_slice())
    }

    /// Formulas that directly read `cell`.
    pub fn direct_dependents(&self, cell: CellRef) -> Vec<CellRef> {
        self.precedents
            .iter()
            .filter(|(f, _)| **f != cell)
            .filter(|(_, ps)| ps.iter().any(|p| p.contains(cell)))
            .map(|(f, _)| *f)
            .collect()
    }

    /// Does this formula reference itself, directly or transitively?
    pub fn is_circular(&self, start: CellRef) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        let mut first = true;
        while let Some(cur) = stack.pop() {
            if !first && cur == start {
                return true;
            }
            first = false;
            if !seen.insert(cur) {
                continue;
            }
            // Follow edges from `cur` to the formulas it reads.
            if let Some(ps) = self.precedents.get(&cur) {
                for p in ps {
                    match p {
                        Precedent::Cell(c) => {
                            if *c == start {
                                return true;
                            }
                            if self.precedents.contains_key(c) {
                                stack.push(*c);
                            }
                        }
                        Precedent::Range(_, _) => {
                            // A range may cover other formula cells; check each
                            // formula we know about for membership.
                            for f in self.precedents.keys() {
                                if p.contains(*f) {
                                    if *f == start {
                                        return true;
                                    }
                                    stack.push(*f);
                                }
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Every formula affected by a change to `changed`, in an order safe to
    /// evaluate (each cell appears after everything it depends on).
    ///
    /// Returns `Err` with the cells involved if a cycle is detected.
    pub fn recalc_order(&self, changed: CellRef) -> Result<Vec<CellRef>, Vec<CellRef>> {
        // 1. Find the affected set by walking dependents transitively.
        let mut affected = HashSet::new();
        let mut queue = VecDeque::new();
        for d in self.direct_dependents(changed) {
            if affected.insert(d) {
                queue.push_back(d);
            }
        }
        while let Some(cur) = queue.pop_front() {
            for d in self.direct_dependents(cur) {
                if affected.insert(d) {
                    queue.push_back(d);
                }
            }
        }
        if affected.is_empty() {
            return Ok(Vec::new());
        }

        // 2. Kahn's algorithm over the induced subgraph.
        let nodes: Vec<CellRef> = affected.iter().copied().collect();
        let mut indegree: HashMap<CellRef, usize> = nodes.iter().map(|n| (*n, 0)).collect();
        let mut edges: HashMap<CellRef, Vec<CellRef>> = HashMap::new();

        for &n in &nodes {
            if let Some(ps) = self.precedents.get(&n) {
                for &m in &nodes {
                    if m != n && ps.iter().any(|p| p.contains(m)) {
                        // n reads m, so m must be evaluated first.
                        edges.entry(m).or_default().push(n);
                        *indegree.get_mut(&n).unwrap() += 1;
                    }
                }
            }
        }

        let mut ready: VecDeque<CellRef> =
            nodes.iter().filter(|n| indegree[n] == 0).copied().collect();
        // Deterministic output makes tests and debugging sane.
        let mut ready: Vec<CellRef> = ready.drain(..).collect();
        ready.sort();
        let mut ready: VecDeque<CellRef> = ready.into();

        let mut order = Vec::with_capacity(nodes.len());
        while let Some(cur) = ready.pop_front() {
            order.push(cur);
            if let Some(outs) = edges.get(&cur) {
                let mut newly: Vec<CellRef> = Vec::new();
                for &next in outs {
                    let d = indegree.get_mut(&next).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        newly.push(next);
                    }
                }
                newly.sort();
                for n in newly {
                    ready.push_back(n);
                }
            }
        }

        if order.len() != nodes.len() {
            // Whatever never reached indegree 0 is part of a cycle.
            let stuck: Vec<CellRef> = nodes.into_iter().filter(|n| !order.contains(n)).collect();
            return Err(stuck);
        }
        Ok(order)
    }

    /// Full evaluation order for every formula — used on load.
    pub fn full_order(&self) -> Result<Vec<CellRef>, Vec<CellRef>> {
        let nodes: Vec<CellRef> = self.precedents.keys().copied().collect();
        let mut indegree: HashMap<CellRef, usize> = nodes.iter().map(|n| (*n, 0)).collect();
        let mut edges: HashMap<CellRef, Vec<CellRef>> = HashMap::new();

        for &n in &nodes {
            if let Some(ps) = self.precedents.get(&n) {
                for &m in &nodes {
                    if m != n && ps.iter().any(|p| p.contains(m)) {
                        edges.entry(m).or_default().push(n);
                        *indegree.get_mut(&n).unwrap() += 1;
                    }
                }
            }
        }

        let mut ready: Vec<CellRef> = nodes.iter().filter(|n| indegree[n] == 0).copied().collect();
        ready.sort();
        let mut ready: VecDeque<CellRef> = ready.into();

        let mut order = Vec::with_capacity(nodes.len());
        while let Some(cur) = ready.pop_front() {
            order.push(cur);
            if let Some(outs) = edges.get(&cur) {
                let mut newly = Vec::new();
                for &next in outs {
                    let d = indegree.get_mut(&next).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        newly.push(next);
                    }
                }
                newly.sort();
                for n in newly {
                    ready.push_back(n);
                }
            }
        }

        if order.len() != nodes.len() {
            let stuck: Vec<CellRef> = nodes.into_iter().filter(|n| !order.contains(n)).collect();
            return Err(stuck);
        }
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn cr(row: u32, col: u32) -> CellRef {
        CellRef::new(row, col)
    }

    fn graph(entries: &[(CellRef, &str)]) -> DepGraph {
        let mut g = DepGraph::new();
        for (cell, src) in entries {
            g.set_formula(*cell, &parse(src).unwrap());
        }
        g
    }

    #[test]
    fn collects_cell_and_range_precedents() {
        let mut p = Vec::new();
        collect_precedents(&parse("=A1+SUM(B1:B10)").unwrap(), &mut p);
        assert_eq!(p.len(), 2);
        assert!(p.contains(&Precedent::Cell(cr(0, 0))));
        assert!(p.contains(&Precedent::Range(cr(0, 1), cr(9, 1))));
    }

    #[test]
    fn range_contains_is_rectangular() {
        let r = Precedent::Range(cr(0, 0), cr(9, 2));
        assert!(r.contains(cr(0, 0)));
        assert!(r.contains(cr(9, 2)));
        assert!(r.contains(cr(5, 1)));
        assert!(!r.contains(cr(10, 1)));
        assert!(!r.contains(cr(5, 3)));
    }

    #[test]
    fn direct_dependents_via_cell_and_range() {
        // C1 reads A1 directly; C2 reads a range covering A1.
        let g = graph(&[(cr(0, 2), "=A1*2"), (cr(1, 2), "=SUM(A1:A10)")]);
        let mut deps = g.direct_dependents(cr(0, 0));
        deps.sort();
        assert_eq!(deps, vec![cr(0, 2), cr(1, 2)]);
        // A cell outside both is unaffected.
        assert!(g.direct_dependents(cr(50, 0)).is_empty());
    }

    #[test]
    fn recalc_order_is_dependency_safe() {
        // B1 = A1*2, C1 = B1+1  =>  editing A1 must recompute B1 before C1.
        let g = graph(&[(cr(0, 1), "=A1*2"), (cr(0, 2), "=B1+1")]);
        let order = g.recalc_order(cr(0, 0)).unwrap();
        assert_eq!(order, vec![cr(0, 1), cr(0, 2)]);
    }

    #[test]
    fn recalc_order_handles_diamond() {
        // B1 and C1 both read A1; D1 reads both. D1 must come last.
        let g = graph(&[
            (cr(0, 1), "=A1+1"),
            (cr(0, 2), "=A1+2"),
            (cr(0, 3), "=B1+C1"),
        ]);
        let order = g.recalc_order(cr(0, 0)).unwrap();
        assert_eq!(order.len(), 3);
        let pos = |c: CellRef| order.iter().position(|x| *x == c).unwrap();
        assert!(pos(cr(0, 1)) < pos(cr(0, 3)));
        assert!(pos(cr(0, 2)) < pos(cr(0, 3)));
    }

    #[test]
    fn unaffected_edit_produces_empty_order() {
        let g = graph(&[(cr(0, 1), "=A1*2")]);
        assert!(g.recalc_order(cr(99, 99)).unwrap().is_empty());
    }

    #[test]
    fn detects_direct_self_reference() {
        let g = graph(&[(cr(0, 0), "=A1+1")]);
        assert!(g.is_circular(cr(0, 0)));
    }

    #[test]
    fn detects_mutual_cycle() {
        // A1 = B1+1, B1 = A1+1
        let g = graph(&[(cr(0, 0), "=B1+1"), (cr(0, 1), "=A1+1")]);
        assert!(g.is_circular(cr(0, 0)));
        assert!(g.is_circular(cr(0, 1)));
        assert!(g.full_order().is_err());
    }

    #[test]
    fn detects_long_cycle() {
        // A1 -> B1 -> C1 -> A1
        let g = graph(&[
            (cr(0, 0), "=C1+1"),
            (cr(0, 1), "=A1+1"),
            (cr(0, 2), "=B1+1"),
        ]);
        assert!(g.is_circular(cr(0, 0)));
        let err = g.full_order().unwrap_err();
        assert_eq!(err.len(), 3, "all three cells are part of the cycle");
    }

    #[test]
    fn detects_cycle_through_a_range() {
        // A5 = SUM(A1:A10) includes itself.
        let g = graph(&[(cr(4, 0), "=SUM(A1:A10)")]);
        assert!(g.is_circular(cr(4, 0)));
    }

    #[test]
    fn acyclic_chain_is_not_circular() {
        let g = graph(&[(cr(0, 1), "=A1*2"), (cr(0, 2), "=B1+1")]);
        assert!(!g.is_circular(cr(0, 1)));
        assert!(!g.is_circular(cr(0, 2)));
        assert!(g.full_order().is_ok());
    }

    #[test]
    fn full_order_evaluates_deep_chain_in_sequence() {
        // A chain B1<-C1<-D1<-E1 must come out in exactly that order.
        let g = graph(&[
            (cr(0, 1), "=A1+1"),
            (cr(0, 2), "=B1+1"),
            (cr(0, 3), "=C1+1"),
            (cr(0, 4), "=D1+1"),
        ]);
        let order = g.full_order().unwrap();
        assert_eq!(order, vec![cr(0, 1), cr(0, 2), cr(0, 3), cr(0, 4)]);
    }

    #[test]
    fn graph_holds_only_formulas_not_data() {
        // The scale claim: a huge sheet with few formulas is a tiny graph.
        let g = graph(&[(cr(0, 9), "=SUM(A1:A200000000)")]);
        assert_eq!(g.len(), 1, "200M data cells must not enter the graph");
        // And a change deep in that range still resolves via rectangle test.
        assert_eq!(g.direct_dependents(cr(199_999_999, 0)), vec![cr(0, 9)]);
    }

    #[test]
    fn replacing_a_formula_replaces_its_edges() {
        let mut g = DepGraph::new();
        g.set_formula(cr(0, 1), &parse("=A1*2").unwrap());
        assert_eq!(g.direct_dependents(cr(0, 0)), vec![cr(0, 1)]);
        // Repoint B1 at C1 instead; it must no longer depend on A1.
        g.set_formula(cr(0, 1), &parse("=C1*2").unwrap());
        assert!(g.direct_dependents(cr(0, 0)).is_empty());
        assert_eq!(g.direct_dependents(cr(0, 2)), vec![cr(0, 1)]);
    }
}
