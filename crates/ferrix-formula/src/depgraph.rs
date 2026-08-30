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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use ferrix_core::{CellRef, SheetCell, SheetId};

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

/// A precedent plus the sheet it lives in.
///
/// The sheet is kept *beside* [`Precedent`] rather than inside it so that the
/// rectangle-containment test stays a pure geometry question on `CellRef` —
/// the fast path that lets `SUM(A1:A200000000)` be one comparison instead of
/// 200M edges. Sheet identity is checked first and costs one `u32` compare.
pub type ScopedPrecedent = (SheetId, Precedent);

/// The workbook's sheets, in TAB ORDER, as the graph needs to see them.
///
/// Two questions are asked of it, and they are different questions:
///
/// * `id` — what sheet does this NAME mean? (`Sheet2!A1`)
/// * `span` — which sheets lie in this RUN? (`Sheet1:Sheet3!A1`)
///
/// The second cannot be answered by a name lookup, which is why the graph
/// takes this rather than the bare `Fn(&str) -> Option<SheetId>` closure it
/// used to: a closure could only ever answer the first, so a 3-D reference
/// would silently contribute no edges and its formula would never recalculate.
///
/// Cost is one small entry per SHEET — a workbook quantity, never a row one.
#[derive(Debug, Default, Clone)]
pub struct SheetIndex {
    /// (id, name) in tab order.
    order: Vec<(SheetId, String)>,
}

impl SheetIndex {
    /// Build from the tab strip, left to right.
    pub fn new(order: Vec<(SheetId, String)>) -> Self {
        Self { order }
    }

    /// Resolve a sheet name, case-insensitively — Excel's rule, and the one
    /// `sheet2!A1` needs to find `Sheet2`.
    pub fn id(&self, name: &str) -> Option<SheetId> {
        self.order
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(name))
            .map(|(id, _)| *id)
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.order
            .iter()
            .position(|(_, n)| n.eq_ignore_ascii_case(name))
    }

    /// Every sheet in the inclusive tab-order run `first..=last`.
    ///
    /// Written backwards (`Sheet3:Sheet1`) is the same run, as in Excel. An
    /// endpoint that does not exist yields nothing at all rather than a
    /// half-open run: a 3-D reference with a broken endpoint is `#REF!`, and
    /// quietly summing "as much of it as still resolves" would be a wrong
    /// number that looks right.
    pub fn span(&self, first: &str, last: &str) -> Vec<SheetId> {
        let (Some(a), Some(b)) = (self.position(first), self.position(last)) else {
            return Vec::new();
        };
        let (lo, hi) = (a.min(b), a.max(b));
        self.order[lo..=hi].iter().map(|(id, _)| *id).collect()
    }
}

/// Walk an expression and collect everything it reads *within its own sheet*.
///
/// Cross-sheet references (`Sheet2!A1`) are deliberately skipped here: this
/// entry point has no way to turn a sheet NAME into a [`SheetId`]. Callers
/// that hold a workbook use [`collect_precedents_scoped`] instead.
pub fn collect_precedents(expr: &Expr, out: &mut Vec<Precedent>) {
    match expr {
        Expr::Ref(c) => out.push(Precedent::Cell(*c)),
        Expr::Range(a, b) => out.push(Precedent::Range(*a, *b)),
        Expr::XRef(_, _) | Expr::XRange(_, _, _) | Expr::X3D(_, _, _, _) => {}
        // `A1#` depends on the spilling HOST at its anchor: any change that
        // re-plans that host's spill updates the host cell, so tracking the
        // anchor cell is exactly the edge that makes a spill-range consumer
        // recalculate. The spill's extent is dynamic and unknown here, but it
        // never needs to be — the host cell is the one stable dependency.
        Expr::SpillRange(anchor) => out.push(Precedent::Cell(*anchor)),
        Expr::Intersect(inner) | Expr::Unary(_, inner) => collect_precedents(inner, out),
        Expr::Binary(_, l, r) => {
            collect_precedents(l, out);
            collect_precedents(r, out);
        }
        Expr::Call(_, args) => {
            for a in args {
                collect_precedents(a, out);
            }
        }
        // A lexical variable is a LET/LAMBDA name, not a cell — no precedent.
        Expr::Var(_) => {}
        // LET/LAMBDA/Apply carry references inside their value expressions,
        // bodies, and arguments; those are the real dependencies. Parameter and
        // binding NAMES are lexical and contribute nothing.
        Expr::Let(bindings, body) => {
            for (_, value) in bindings {
                collect_precedents(value, out);
            }
            collect_precedents(body, out);
        }
        Expr::Lambda(_, body) => collect_precedents(body, out),
        Expr::Apply(callee, args) => {
            collect_precedents(callee, out);
            for a in args {
                collect_precedents(a, out);
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Error(_) => {}
    }
}

/// Walk an expression, resolving sheet-qualified references through `sheets`.
///
/// Unqualified references belong to `home`. A name `sheets` cannot place is
/// dropped from the graph — the formula will evaluate to `#REF!`, and a
/// dangling edge to a sheet that does not exist would only corrupt ordering.
pub fn collect_precedents_scoped(
    expr: &Expr,
    home: SheetId,
    sheets: &SheetIndex,
    out: &mut Vec<ScopedPrecedent>,
) {
    match expr {
        Expr::Ref(c) => out.push((home, Precedent::Cell(*c))),
        Expr::Range(a, b) => out.push((home, Precedent::Range(*a, *b))),
        Expr::XRef(name, c) => {
            if let Some(id) = sheets.id(name) {
                out.push((id, Precedent::Cell(*c)));
            }
        }
        Expr::XRange(name, a, b) => {
            if let Some(id) = sheets.id(name) {
                out.push((id, Precedent::Range(*a, *b)));
            }
        }
        // One rectangle PER SHEET in the run, not per cell: a 3-D SUM over
        // three 200M-row columns is three entries.
        Expr::X3D(first, last, a, b) => {
            for id in sheets.span(first, last) {
                out.push((id, Precedent::Range(*a, *b)));
            }
        }
        // A spill-range tracks its host on the home sheet (see the unscoped
        // twin). The `#` operator is same-sheet only — there is no
        // `Sheet2!A1#` form in the grammar — so `home` is always the right
        // scope for the anchor.
        Expr::SpillRange(anchor) => out.push((home, Precedent::Cell(*anchor))),
        Expr::Intersect(inner) | Expr::Unary(_, inner) => {
            collect_precedents_scoped(inner, home, sheets, out)
        }
        Expr::Binary(_, l, r) => {
            collect_precedents_scoped(l, home, sheets, out);
            collect_precedents_scoped(r, home, sheets, out);
        }
        Expr::Call(_, args) => {
            for a in args {
                collect_precedents_scoped(a, home, sheets, out);
            }
        }
        // Same as the unscoped twin: lexical names carry no dependency, but the
        // references inside LET values, LAMBDA bodies, and application arguments
        // do.
        Expr::Var(_) => {}
        Expr::Let(bindings, body) => {
            for (_, value) in bindings {
                collect_precedents_scoped(value, home, sheets, out);
            }
            collect_precedents_scoped(body, home, sheets, out);
        }
        Expr::Lambda(_, body) => collect_precedents_scoped(body, home, sheets, out),
        Expr::Apply(callee, args) => {
            collect_precedents_scoped(callee, home, sheets, out);
            for a in args {
                collect_precedents_scoped(a, home, sheets, out);
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Error(_) => {}
    }
}

#[inline]
fn scoped_contains(p: &ScopedPrecedent, target: SheetCell) -> bool {
    p.0 == target.sheet && p.1.contains(target.cell)
}

/// A set of formula cells, indexed so that "which of these does this rectangle
/// cover?" is a binary search plus a short scan.
///
/// Every range precedent asks that question, and the graph used to answer it by
/// rescanning EVERY formula key in the workbook. With `F` formulas that is O(F)
/// per range, O(F²)–O(F³) over a walk — so a workbook of thousands of
/// `=SUM(A1:A1000000)` plus many small formulas inside those ranges turned
/// recalculation into a CPU denial of service (issue #59). The rescans
/// terminated, so this is a cost bug, not a hang.
///
/// Cost is one `CellRef` per FORMULA, per sheet — a workbook quantity, never a
/// row one. A 200M-row sheet with 50 formulas indexes 50 entries, so the scale
/// invariant is untouched.
///
/// Built on demand rather than cached in the graph: rebuilding is O(F log F),
/// already far under the O(F²) it replaces, and a cached copy would need
/// invalidating on every `set_formula_at` / `remove_at` / `remove_sheet` — a
/// staleness bug that would mis-order a recalculation, which is a wrong-number
/// bug rather than a slow one.
#[derive(Debug, Default)]
struct CellIndex {
    /// sheet -> column -> that column's formula rows, ascending.
    ///
    /// Column-major, and the `BTreeMap` is the point: a rectangle is a column
    /// span crossed with a row span, so the lookup walks only the columns that
    /// BOTH the range covers and a formula actually occupies, then binary
    /// searches the row window inside each.
    ///
    /// A row-major layout was tried first and is a trap — `=SUM(A1:A1000000)`
    /// is one column but a million rows, so seeking to the row band and
    /// scanning it touches every formula in those rows regardless of column,
    /// which is the same O(F) rescan with extra steps.
    by_sheet: HashMap<SheetId, BTreeMap<u32, Vec<u32>>>,
}

impl CellIndex {
    fn build(cells: impl Iterator<Item = SheetCell>) -> Self {
        let mut by_sheet: HashMap<SheetId, BTreeMap<u32, Vec<u32>>> = HashMap::new();
        for sc in cells {
            by_sheet
                .entry(sc.sheet)
                .or_default()
                .entry(sc.cell.col)
                .or_default()
                .push(sc.cell.row);
        }
        for cols in by_sheet.values_mut() {
            for rows in cols.values_mut() {
                rows.sort_unstable();
            }
        }
        Self { by_sheet }
    }

    /// Call `f` for every indexed cell that `p` covers, within `sheet`.
    ///
    /// The membership test is exactly the one [`Precedent::contains`] applies,
    /// so this is a pure speed change: the same cells come back, and a range
    /// left un-normalized (start > end) yields nothing from both, as before.
    ///
    /// Work is proportional to the cells actually found, plus one binary search
    /// per occupied column inside the span. That per-column term is capped by
    /// the sheet's column count, never by the row count — so a range a million
    /// rows tall costs the same as a range one row tall.
    fn for_each_in(&self, sheet: SheetId, p: &Precedent, mut f: impl FnMut(CellRef)) {
        let Some(cols) = self.by_sheet.get(&sheet) else {
            return;
        };
        match p {
            Precedent::Cell(c) => {
                if let Some(rows) = cols.get(&c.col) {
                    if rows.binary_search(&c.row).is_ok() {
                        f(*c);
                    }
                }
            }
            Precedent::Range(a, b) => {
                if a.row > b.row || a.col > b.col {
                    return;
                }
                for (&col, rows) in cols.range(a.col..=b.col) {
                    let start = rows.partition_point(|&r| r < a.row);
                    for &row in &rows[start..] {
                        if row > b.row {
                            break;
                        }
                        f(CellRef::new(row, col));
                    }
                }
            }
        }
    }
}

/// Formula dependency graph, keyed workbook-wide.
///
/// Nodes are [`SheetCell`]s, so a chain that hops between sheets is the same
/// graph as one that does not — ordering and cycle detection get sheet-crossing
/// correctness for free rather than needing a second, per-workbook pass.
#[derive(Debug, Default)]
pub struct DepGraph {
    /// formula cell -> what it reads
    precedents: HashMap<SheetCell, Vec<ScopedPrecedent>>,
    /// formula cell -> the defined names its SOURCE TEXT mentions, upper-cased.
    ///
    /// Names resolve to plain ranges in the parser, so by the time `precedents`
    /// is built the name has vanished — which is exactly what makes a named
    /// range cost the same as an explicit one. But a rename or a delete has to
    /// find the formulas that mention a name, and rescanning every formula's
    /// text on each edit would be O(workbook). Recording the (tiny) list of
    /// words alongside the edges makes that a lookup instead.
    ///
    /// Entries are kept even when the name is not defined: a formula reading
    /// `=SUM(Sales)` while `Sales` is undefined is exactly the formula that
    /// must be revisited when `Sales` is later defined.
    name_uses: HashMap<SheetCell, Vec<String>>,
    /// formula cell -> the SHEET NAMES its SOURCE TEXT mentions.
    ///
    /// Kept for the same reason `name_uses` is, and it is NOT derivable from
    /// `precedents`: a formula naming a sheet that does not exist has no edges
    /// at all, and that is precisely the formula a rename has to find — it is
    /// the one showing `#REF!` that a rename could repair, and the one a
    /// delete just broke.
    ///
    /// Bounded by the number of qualifiers in a formula, which is a property
    /// of the formula's text, never of the sheet's height.
    sheet_uses: HashMap<SheetCell, Vec<String>>,
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

    // --- sheet-aware API -------------------------------------------------

    /// Register (or replace) a formula's dependencies, resolving any
    /// sheet-qualified references through `sheets`.
    pub fn set_formula_at(&mut self, at: SheetCell, expr: &Expr, sheets: &SheetIndex) {
        let mut p = Vec::new();
        collect_precedents_scoped(expr, at.sheet, sheets, &mut p);
        self.precedents.insert(at, p);
    }

    pub fn remove_at(&mut self, at: SheetCell) {
        self.precedents.remove(&at);
        self.name_uses.remove(&at);
        self.sheet_uses.remove(&at);
    }

    /// Forget every formula belonging to `sheet` — used when a sheet is
    /// deleted, so its nodes cannot keep participating in recalculation.
    pub fn remove_sheet(&mut self, sheet: SheetId) {
        self.precedents.retain(|k, _| k.sheet != sheet);
        self.name_uses.retain(|k, _| k.sheet != sheet);
        self.sheet_uses.retain(|k, _| k.sheet != sheet);
    }

    // --- defined names ----------------------------------------------------

    /// Record which defined names a formula's SOURCE TEXT mentions.
    ///
    /// Called with the raw text rather than the parsed tree because the parser
    /// has already replaced every name with the range it stands for — the name
    /// only exists in the text.
    pub fn set_name_uses(&mut self, at: SheetCell, src: &str) {
        let names = crate::names::names_in(src);
        if names.is_empty() {
            self.name_uses.remove(&at);
        } else {
            self.name_uses.insert(at, names);
        }
    }

    /// The defined names a formula mentions, upper-cased.
    pub fn name_uses_at(&self, at: SheetCell) -> &[String] {
        self.name_uses.get(&at).map_or(&[], |v| v.as_slice())
    }

    /// Every formula in the workbook whose text mentions `ident`.
    ///
    /// This is what a rename rewrites and what a delete invalidates. Sorted so
    /// a rename produces a deterministic sequence of edits.
    pub fn cells_using_name(&self, ident: &str) -> Vec<SheetCell> {
        let want = ident.to_ascii_uppercase();
        let mut v: Vec<SheetCell> = self
            .name_uses
            .iter()
            .filter(|(_, names)| names.contains(&want))
            .map(|(at, _)| *at)
            .collect();
        v.sort();
        v
    }

    /// Rewrite a recorded name use after a rename, so the graph keeps agreeing
    /// with the formula text the caller just rewrote.
    pub fn rename_name_use(&mut self, old: &str, new: &str) {
        let (old_u, new_u) = (old.to_ascii_uppercase(), new.to_ascii_uppercase());
        for names in self.name_uses.values_mut() {
            for n in names.iter_mut() {
                if *n == old_u {
                    *n = new_u.clone();
                }
            }
        }
    }

    // --- sheet uses -------------------------------------------------------

    /// Record which SHEETS a formula's source text names.
    ///
    /// Called with the raw text, not the tree, because a formula naming a
    /// sheet that does not exist parses to an `XRef` the graph then drops —
    /// leaving no trace of the name anywhere in the edges.
    pub fn set_sheet_uses(&mut self, at: SheetCell, src: &str) {
        let sheets = crate::names::sheets_in(src);
        if sheets.is_empty() {
            self.sheet_uses.remove(&at);
        } else {
            self.sheet_uses.insert(at, sheets);
        }
    }

    /// The sheets a formula names, in the spelling written.
    pub fn sheet_uses_at(&self, at: SheetCell) -> &[String] {
        self.sheet_uses.get(&at).map_or(&[], |v| v.as_slice())
    }

    /// Every formula in the workbook whose TEXT names `sheet`.
    ///
    /// This is what a sheet rename rewrites and what a sheet delete breaks.
    /// Sorted, so a rename produces a deterministic sequence of edits.
    pub fn cells_using_sheet(&self, sheet: &str) -> Vec<SheetCell> {
        let mut v: Vec<SheetCell> = self
            .sheet_uses
            .iter()
            .filter(|(_, names)| names.iter().any(|n| n.eq_ignore_ascii_case(sheet)))
            .map(|(at, _)| *at)
            .collect();
        v.sort();
        v
    }

    /// Every formula cell registered for `sheet`.
    pub fn cells_in(&self, sheet: SheetId) -> Vec<SheetCell> {
        let mut v: Vec<SheetCell> = self
            .precedents
            .keys()
            .copied()
            .filter(|k| k.sheet == sheet)
            .collect();
        v.sort();
        v
    }

    pub fn precedents_at(&self, at: SheetCell) -> Option<&[ScopedPrecedent]> {
        self.precedents.get(&at).map(|v| v.as_slice())
    }

    /// Formulas anywhere in the workbook that directly read `at`.
    pub fn direct_dependents_at(&self, at: SheetCell) -> Vec<SheetCell> {
        self.precedents
            .iter()
            .filter(|(f, _)| **f != at)
            .filter(|(_, ps)| ps.iter().any(|p| scoped_contains(p, at)))
            .map(|(f, _)| *f)
            .collect()
    }

    /// Does this formula reference itself, directly or transitively —
    /// including via a chain that leaves and re-enters its sheet?
    pub fn is_circular_at(&self, start: SheetCell) -> bool {
        let index = CellIndex::build(self.precedents.keys().copied());
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        let mut first = true;
        while let Some(cur) = stack.pop() {
            if !first && cur == start {
                return true;
            }
            first = false;
            // `seen` is what makes this terminate rather than spin around a
            // cycle forever — including one that spans sheets.
            if !seen.insert(cur) {
                continue;
            }
            if let Some(ps) = self.precedents.get(&cur) {
                for (sheet, p) in ps {
                    match p {
                        Precedent::Cell(c) => {
                            let target = SheetCell::new(*sheet, *c);
                            if target == start {
                                return true;
                            }
                            if self.precedents.contains_key(&target) {
                                stack.push(target);
                            }
                        }
                        Precedent::Range(_, _) => {
                            // A range may cover other formula cells; check each
                            // formula in THAT sheet for membership. Indexed, so
                            // this touches the covered formulas instead of
                            // rescanning every formula in the workbook (#59).
                            let mut hit_start = false;
                            index.for_each_in(*sheet, p, |c| {
                                let f = SheetCell::new(*sheet, c);
                                if f == start {
                                    hit_start = true;
                                }
                                stack.push(f);
                            });
                            if hit_start {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Does `target` transitively depend on `from` — directly, or through any
    /// chain of intermediate formulas, however many hops and however many
    /// sheets it crosses?
    ///
    /// This is a PRECEDENT walk (what does `target` read), not a dependent
    /// walk, because Goal Seek is asked the question the other feature graphs
    /// answer backwards: "if I changed `from`, would `target` move at all?"
    /// Answering that by first computing `recalc_order_at(from)` and checking
    /// membership would work too, but it computes the full downstream set of
    /// `from` — every dependent, however unrelated to `target` — where this
    /// walk can stop the instant it finds `from`, and does not need `from` to
    /// be a formula cell (it usually is not: Goal Seek's "changing cell" is
    /// typically a plain input).
    ///
    /// `target == from` is not a dependency: a cell does not transitively
    /// depend on itself just by existing.
    pub fn depends_on_at(&self, target: SheetCell, from: SheetCell) -> bool {
        if target == from {
            return false;
        }
        let index = CellIndex::build(self.precedents.keys().copied());
        let mut seen = HashSet::new();
        let mut stack = vec![target];
        while let Some(cur) = stack.pop() {
            // `seen` is what makes this terminate on a cycle rather than
            // spin — the same guard `is_circular_at` uses.
            if !seen.insert(cur) {
                continue;
            }
            let Some(ps) = self.precedents.get(&cur) else {
                continue;
            };
            for (sheet, p) in ps {
                match p {
                    Precedent::Cell(c) => {
                        let refd = SheetCell::new(*sheet, *c);
                        if refd == from {
                            return true;
                        }
                        if self.precedents.contains_key(&refd) {
                            stack.push(refd);
                        }
                    }
                    Precedent::Range(_, _) => {
                        if *sheet == from.sheet && p.contains(from.cell) {
                            return true;
                        }
                        // A range may also cover other formula cells whose
                        // OWN precedents need walking (a formula inside the
                        // summed range that itself reads `from` indirectly).
                        // Indexed rather than a full rescan of every formula
                        // key in the workbook (#59).
                        index.for_each_in(*sheet, p, |c| {
                            stack.push(SheetCell::new(*sheet, c));
                        });
                    }
                }
            }
        }
        false
    }

    /// Single-sheet convenience for [`DepGraph::depends_on_at`].
    pub fn depends_on(&self, target: CellRef, from: CellRef) -> bool {
        self.depends_on_at(SheetCell::main(target), SheetCell::main(from))
    }

    /// Every formula affected by a change to `changed`, in an order safe to
    /// evaluate. Spans sheets: editing Sheet1!A1 returns the Sheet2 formulas
    /// that read it, correctly ordered against everything else.
    pub fn recalc_order_at(&self, changed: SheetCell) -> Result<Vec<SheetCell>, Vec<SheetCell>> {
        let mut affected = HashSet::new();
        let mut queue = VecDeque::new();
        for d in self.direct_dependents_at(changed) {
            if affected.insert(d) {
                queue.push_back(d);
            }
        }
        while let Some(cur) = queue.pop_front() {
            for d in self.direct_dependents_at(cur) {
                if affected.insert(d) {
                    queue.push_back(d);
                }
            }
        }
        if affected.is_empty() {
            return Ok(Vec::new());
        }
        let nodes: Vec<SheetCell> = affected.into_iter().collect();
        self.topo_sort(nodes)
    }

    /// Full evaluation order for every formula in the workbook — used on load.
    pub fn full_order_all(&self) -> Result<Vec<SheetCell>, Vec<SheetCell>> {
        let nodes: Vec<SheetCell> = self.precedents.keys().copied().collect();
        self.topo_sort(nodes)
    }

    /// Kahn's algorithm over the induced subgraph on `nodes`.
    ///
    /// Anything that never reaches indegree 0 is, by definition, in a cycle —
    /// which is how a two-sheet cycle is *detected* rather than hung on: the
    /// algorithm is a finite drain, not a traversal that can loop.
    fn topo_sort(&self, nodes: Vec<SheetCell>) -> Result<Vec<SheetCell>, Vec<SheetCell>> {
        let mut indegree: HashMap<SheetCell, usize> = nodes.iter().map(|n| (*n, 0)).collect();
        let mut edges: HashMap<SheetCell, Vec<SheetCell>> = HashMap::new();

        // Indexed over `nodes` — the induced subgraph — not the whole graph,
        // so an edge can only ever land on a node this sort actually owns.
        // Replaces the O(F²) `for n in nodes { for m in nodes { ... } }` double
        // loop that made ordering a CPU denial of service on range-heavy
        // workbooks (#59).
        let index = CellIndex::build(nodes.iter().copied());

        // Reused across nodes so a wide range does not reallocate per formula.
        let mut covered: HashSet<SheetCell> = HashSet::new();
        for &n in &nodes {
            if let Some(ps) = self.precedents.get(&n) {
                // DEDUPED, and this is load-bearing: the old double loop asked
                // `ps.iter().any(...)`, so two precedents covering the same `m`
                // (`=A1+SUM(A1:A9)`) produced ONE edge. Pushing one per
                // precedent would inflate `m`'s indegree above the number of
                // times the drain can decrement it, stranding it at non-zero —
                // reported as a false circular reference.
                covered.clear();
                for (sheet, p) in ps {
                    index.for_each_in(*sheet, p, |c| {
                        covered.insert(SheetCell::new(*sheet, c));
                    });
                }
                for &m in covered.iter() {
                    if m != n {
                        // n reads m, so m must be evaluated first.
                        edges.entry(m).or_default().push(n);
                        *indegree.get_mut(&n).unwrap() += 1;
                    }
                }
            }
        }

        // Deterministic output makes tests and debugging sane.
        let mut ready: Vec<SheetCell> =
            nodes.iter().filter(|n| indegree[n] == 0).copied().collect();
        ready.sort();
        let mut ready: VecDeque<SheetCell> = ready.into();

        let mut order = Vec::with_capacity(nodes.len());
        while let Some(cur) = ready.pop_front() {
            order.push(cur);
            if let Some(outs) = edges.get(&cur) {
                let mut newly: Vec<SheetCell> = Vec::new();
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
            // Set membership, not `order.contains(n)`: that is a linear scan
            // inside a filter over every node, so reporting a cycle was itself
            // O(F²) — the quadratic the rest of this function just removed.
            let placed: HashSet<SheetCell> = order.iter().copied().collect();
            let stuck: Vec<SheetCell> = nodes.into_iter().filter(|n| !placed.contains(n)).collect();
            return Err(stuck);
        }
        Ok(order)
    }

    // --- single-sheet convenience ----------------------------------------
    //
    // Everything below addresses `SheetId::MAIN`. Code that has not been
    // taught about sheets keeps working unchanged, and a workbook that only
    // ever has one sheet produces byte-identical graphs to before.

    /// Register (or replace) a formula's dependencies in the main sheet.
    pub fn set_formula(&mut self, cell: CellRef, expr: &Expr) {
        self.set_formula_at(SheetCell::main(cell), expr, &SheetIndex::default());
    }

    pub fn remove(&mut self, cell: CellRef) {
        self.remove_at(SheetCell::main(cell));
    }

    /// Precedents of a main-sheet formula, with sheet scope stripped.
    #[allow(dead_code)]
    pub fn precedents_of(&self, cell: CellRef) -> Option<Vec<Precedent>> {
        self.precedents
            .get(&SheetCell::main(cell))
            .map(|v| v.iter().map(|(_, p)| *p).collect())
    }

    /// Formulas in the main sheet that directly read `cell`.
    pub fn direct_dependents(&self, cell: CellRef) -> Vec<CellRef> {
        self.direct_dependents_at(SheetCell::main(cell))
            .into_iter()
            .filter(|s| s.sheet == SheetId::MAIN)
            .map(|s| s.cell)
            .collect()
    }

    pub fn is_circular(&self, start: CellRef) -> bool {
        self.is_circular_at(SheetCell::main(start))
    }

    pub fn recalc_order(&self, changed: CellRef) -> Result<Vec<CellRef>, Vec<CellRef>> {
        match self.recalc_order_at(SheetCell::main(changed)) {
            Ok(o) => Ok(strip_main(o)),
            Err(e) => Err(strip_main(e)),
        }
    }

    pub fn full_order(&self) -> Result<Vec<CellRef>, Vec<CellRef>> {
        match self.full_order_all() {
            Ok(o) => Ok(strip_main(o)),
            Err(e) => Err(strip_main(e)),
        }
    }
}

fn strip_main(cells: Vec<SheetCell>) -> Vec<CellRef> {
    cells
        .into_iter()
        .filter(|s| s.sheet == SheetId::MAIN)
        .map(|s| s.cell)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse, parse_with_names};

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

    // --- sheet-aware graph (issue #15) ---

    const S1: SheetId = SheetId::MAIN;
    const S2: SheetId = SheetId(1);
    const S3: SheetId = SheetId(2);

    fn sc(sheet: SheetId, row: u32, col: u32) -> SheetCell {
        SheetCell::new(sheet, CellRef::new(row, col))
    }

    /// Sheet index for a fixed three-sheet workbook, in tab order.
    fn names() -> SheetIndex {
        SheetIndex::new(vec![
            (S1, "Sheet1".into()),
            (S2, "Sheet2".into()),
            (S3, "Sheet3".into()),
        ])
    }

    fn wb_graph(entries: &[(SheetCell, &str)]) -> DepGraph {
        let mut g = DepGraph::new();
        let names = names();
        for (at, src) in entries {
            g.set_formula_at(*at, &parse(src).unwrap(), &names);
        }
        g
    }

    #[test]
    fn cross_sheet_precedents_are_scoped_to_the_named_sheet() {
        let g = wb_graph(&[(sc(S1, 0, 1), "=Sheet2!A1*2")]);
        // The dependent is found from the SHEET2 cell...
        assert_eq!(g.direct_dependents_at(sc(S2, 0, 0)), vec![sc(S1, 0, 1)]);
        // ...and NOT from the same coordinates on Sheet1.
        assert!(g.direct_dependents_at(sc(S1, 0, 0)).is_empty());
    }

    #[test]
    fn an_unresolvable_sheet_name_creates_no_edge() {
        // A reference to a sheet that does not exist must not leave a dangling
        // edge that could later bind to an unrelated sheet.
        let g = wb_graph(&[(sc(S1, 0, 1), "=Nowhere!A1")]);
        assert_eq!(g.len(), 1, "the formula is still a node");
        assert!(g.precedents_at(sc(S1, 0, 1)).unwrap().is_empty());
        assert!(g.full_order_all().is_ok());
    }

    #[test]
    fn recalc_order_spans_sheets() {
        // Sheet2!B1 = Sheet1!A1*2, Sheet3!C1 = Sheet2!B1+1.
        // Editing Sheet1!A1 must produce both, in that order.
        let g = wb_graph(&[
            (sc(S2, 0, 1), "=Sheet1!A1*2"),
            (sc(S3, 0, 2), "=Sheet2!B1+1"),
        ]);
        let order = g.recalc_order_at(sc(S1, 0, 0)).unwrap();
        assert_eq!(order, vec![sc(S2, 0, 1), sc(S3, 0, 2)]);
    }

    #[test]
    fn a_two_sheet_cycle_is_detected_and_terminates() {
        // Sheet1!A1 = Sheet2!A1 and Sheet2!A1 = Sheet1!A1. The traversal must
        // finish and report, not spin — `is_circular_at`'s `seen` set and the
        // topological drain are both finite by construction.
        let g = wb_graph(&[(sc(S1, 0, 0), "=Sheet2!A1"), (sc(S2, 0, 0), "=Sheet1!A1")]);
        assert!(g.is_circular_at(sc(S1, 0, 0)));
        assert!(g.is_circular_at(sc(S2, 0, 0)));
        let stuck = g.full_order_all().unwrap_err();
        assert_eq!(stuck.len(), 2, "both ends of the loop are reported");
    }

    #[test]
    fn a_three_sheet_cycle_is_detected() {
        let g = wb_graph(&[
            (sc(S1, 0, 0), "=Sheet2!A1"),
            (sc(S2, 0, 0), "=Sheet3!A1"),
            (sc(S3, 0, 0), "=Sheet1!A1"),
        ]);
        assert!(g.is_circular_at(sc(S1, 0, 0)));
        assert_eq!(g.full_order_all().unwrap_err().len(), 3);
    }

    #[test]
    fn same_coordinates_on_different_sheets_are_not_a_cycle() {
        // Sheet1!A1 = Sheet2!A1 alone is a perfectly ordinary chain. Keying
        // the graph on CellRef alone would have called this circular.
        let g = wb_graph(&[(sc(S1, 0, 0), "=Sheet2!A1")]);
        assert!(!g.is_circular_at(sc(S1, 0, 0)));
        assert!(g.full_order_all().is_ok());
    }

    #[test]
    fn a_cross_sheet_range_cycle_is_detected() {
        // Sheet2!A5 sums a Sheet1 range; Sheet1!A3 (inside it) reads back.
        let g = wb_graph(&[
            (sc(S2, 4, 0), "=SUM(Sheet1!A1:A10)"),
            (sc(S1, 2, 0), "=Sheet2!A5"),
        ]);
        assert!(g.is_circular_at(sc(S2, 4, 0)));
        assert!(g.full_order_all().is_err());
    }

    // --- transitive dependency check (Goal Seek, issue #35) --------------

    #[test]
    fn depends_on_is_true_for_a_direct_precedent() {
        let g = graph(&[(cr(0, 1), "=A1*2")]);
        assert!(g.depends_on(cr(0, 1), cr(0, 0)));
    }

    #[test]
    fn depends_on_is_true_several_hops_downstream() {
        // D1 <- C1 <- B1 <- A1. D1 must be seen as depending on A1 even
        // though it never mentions A1 directly.
        let g = graph(&[
            (cr(0, 1), "=A1+1"), // B1 = A1+1
            (cr(0, 2), "=B1+1"), // C1 = B1+1
            (cr(0, 3), "=C1+1"), // D1 = C1+1
        ]);
        assert!(g.depends_on(cr(0, 3), cr(0, 0)));
    }

    #[test]
    fn depends_on_is_false_for_an_unrelated_cell() {
        let g = graph(&[(cr(0, 1), "=A1*2"), (cr(5, 5), "=Z1*3")]);
        // D1 (5,5) reads Z1, not A1 — no path exists.
        assert!(!g.depends_on(cr(5, 5), cr(0, 0)));
        // And the reverse question is false too: A1 does not depend on B1.
        assert!(!g.depends_on(cr(0, 0), cr(0, 1)));
    }

    #[test]
    fn depends_on_a_cell_is_false_for_itself() {
        let g = graph(&[(cr(0, 1), "=A1*2")]);
        assert!(!g.depends_on(cr(0, 1), cr(0, 1)));
    }

    #[test]
    fn depends_on_sees_through_a_range_precedent() {
        // C1 = SUM(A1:A10); B5 is a plain data cell inside that range, so C1
        // depends on B5 even though the formula names a range, not B5 alone.
        let g = graph(&[(cr(0, 2), "=SUM(A1:A10)")]);
        assert!(g.depends_on(cr(0, 2), cr(4, 0))); // A5 is row index 4
    }

    #[test]
    fn depends_on_crosses_sheets_and_terminates_on_a_cycle() {
        // Sheet2!B1 = Sheet1!A1*2; Sheet3!C1 = Sheet2!B1+1.
        // C1 transitively depends on Sheet1!A1.
        let g = wb_graph(&[
            (sc(S2, 0, 1), "=Sheet1!A1*2"),
            (sc(S3, 0, 2), "=Sheet2!B1+1"),
        ]);
        assert!(g.depends_on_at(sc(S3, 0, 2), sc(S1, 0, 0)));
        assert!(!g.depends_on_at(sc(S3, 0, 2), sc(S1, 5, 5)));

        // A two-sheet cycle must not hang the walk.
        let cyc = wb_graph(&[(sc(S1, 0, 0), "=Sheet2!A1"), (sc(S2, 0, 0), "=Sheet1!A1")]);
        assert!(cyc.depends_on_at(sc(S1, 0, 0), sc(S2, 0, 0)));
    }

    #[test]
    fn removing_a_sheet_drops_only_its_formulas() {
        let mut g = wb_graph(&[
            (sc(S1, 0, 0), "=B1"),
            (sc(S2, 0, 0), "=B1"),
            (sc(S2, 1, 0), "=B2"),
        ]);
        assert_eq!(g.len(), 3);
        g.remove_sheet(S2);
        assert_eq!(g.len(), 1);
        assert!(g.cells_in(S2).is_empty());
        assert_eq!(g.cells_in(S1), vec![sc(S1, 0, 0)]);
    }

    #[test]
    fn the_single_sheet_api_addresses_the_main_sheet() {
        // Everything the pre-sheets code did still works, and lands on MAIN.
        let mut g = DepGraph::new();
        g.set_formula(cr(0, 1), &parse("=A1*2").unwrap());
        assert_eq!(g.cells_in(SheetId::MAIN), vec![sc(S1, 0, 1)]);
        assert_eq!(g.direct_dependents(cr(0, 0)), vec![cr(0, 1)]);
        assert_eq!(g.recalc_order(cr(0, 0)).unwrap(), vec![cr(0, 1)]);
        g.remove(cr(0, 1));
        assert!(g.is_empty());
    }

    #[test]
    fn a_bare_collect_ignores_cross_sheet_refs() {
        // `collect_precedents` has no way to resolve a name, so it must skip
        // them rather than inventing a same-sheet edge.
        let mut p = Vec::new();
        collect_precedents(&parse("=A1+Sheet2!B2").unwrap(), &mut p);
        assert_eq!(p, vec![Precedent::Cell(cr(0, 0))]);
    }

    // --- defined names ----------------------------------------------------

    #[test]
    fn a_named_range_produces_the_same_edges_as_the_explicit_range() {
        // The scale invariant, at the graph level: the name is gone by the
        // time edges are built, so the graph cannot tell the two apart — and
        // a 200M-row name is one rectangle, not 200M edges.
        let named = parse_with_names("=SUM(Sales)", &|w| {
            (w == "SALES").then(|| parse("=B2:B1000").unwrap())
        })
        .unwrap();
        let explicit = parse("=SUM(B2:B1000)").unwrap();

        let mut a = DepGraph::new();
        a.set_formula(cr(0, 0), &named);
        let mut b = DepGraph::new();
        b.set_formula(cr(0, 0), &explicit);
        assert_eq!(
            a.precedents_of(cr(0, 0)),
            b.precedents_of(cr(0, 0)),
            "a name must leave no trace in the edge set"
        );
        assert_eq!(
            a.precedents_of(cr(0, 0)).unwrap(),
            vec![Precedent::Range(cr(1, 1), cr(999, 1))]
        );
    }

    #[test]
    fn name_uses_are_recorded_from_the_text_and_found_again() {
        let mut g = DepGraph::new();
        let at = sc(S1, 0, 2);
        g.set_formula_at(at, &parse("=A1").unwrap(), &SheetIndex::default());
        g.set_name_uses(at, "=SUM(Sales)+A1");
        assert_eq!(g.name_uses_at(at), ["SALES"]);
        assert_eq!(g.cells_using_name("sales"), vec![at]);
        assert!(g.cells_using_name("Revenue").is_empty());
    }

    #[test]
    fn name_uses_skip_references_literals_and_calls() {
        let mut g = DepGraph::new();
        let at = sc(S1, 0, 0);
        // A1 is a reference, "Sales" is text, Sheet1! is a qualifier, SUM( is
        // a call, TRUE is a literal. Only Costs is a name.
        g.set_name_uses(at, "=SUM(A1:A9)+Sheet1!B1+\"Sales\"+Costs*TRUE");
        assert_eq!(g.name_uses_at(at), ["COSTS"]);
    }

    #[test]
    fn a_renamed_name_use_follows_the_rewrite() {
        let mut g = DepGraph::new();
        let at = sc(S1, 0, 0);
        g.set_name_uses(at, "=SUM(Sales)");
        g.rename_name_use("Sales", "Revenue");
        assert!(g.cells_using_name("Sales").is_empty());
        assert_eq!(g.cells_using_name("Revenue"), vec![at]);
    }

    #[test]
    fn dropping_a_formula_drops_its_name_uses_too() {
        let mut g = DepGraph::new();
        let at = sc(S1, 0, 0);
        g.set_formula_at(at, &parse("=1").unwrap(), &SheetIndex::default());
        g.set_name_uses(at, "=Sales");
        g.remove_at(at);
        assert!(
            g.cells_using_name("Sales").is_empty(),
            "a deleted formula must not stay a name dependent"
        );

        // And the same when a whole sheet goes.
        let other = sc(S1, 5, 5);
        g.set_name_uses(other, "=Sales");
        g.remove_sheet(S1);
        assert!(g.cells_using_name("Sales").is_empty());
    }

    // --- 3-D references across a sheet run (issue #43) ---

    #[test]
    fn a_three_d_reference_becomes_one_precedent_per_sheet_in_the_run() {
        // THE scale property: a 3-D range is one RECTANGLE per sheet, never
        // one edge per cell. `Sheet1:Sheet3!A1:A1000000` must be 3 entries.
        let at = sc(S1, 0, 5);
        let g = wb_graph(&[(at, "=SUM(Sheet1:Sheet3!A1:A1000000)")]);
        let ps = g.precedents_at(at).expect("registered");
        assert_eq!(ps.len(), 3, "one rectangle per sheet in the run: {ps:?}");
        let sheets: Vec<SheetId> = ps.iter().map(|(s, _)| *s).collect();
        assert_eq!(sheets, vec![S1, S2, S3]);
        for (_, p) in ps {
            assert_eq!(*p, Precedent::Range(cr(0, 0), cr(999_999, 0)));
        }
    }

    #[test]
    fn a_three_d_run_written_backwards_covers_the_same_sheets() {
        let at = sc(S1, 0, 5);
        let g = wb_graph(&[(at, "=SUM(Sheet3:Sheet1!A1)")]);
        let sheets: Vec<SheetId> = g
            .precedents_at(at)
            .unwrap()
            .iter()
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(sheets, vec![S1, S2, S3], "tab order decides, not spelling");
    }

    #[test]
    fn a_partial_three_d_run_covers_only_its_own_sheets() {
        // The control for the test above: if `span` ignored its endpoints and
        // always returned every sheet, that test would pass anyway.
        let at = sc(S1, 0, 5);
        let g = wb_graph(&[(at, "=SUM(Sheet1:Sheet2!A1)")]);
        let sheets: Vec<SheetId> = g
            .precedents_at(at)
            .unwrap()
            .iter()
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(sheets, vec![S1, S2], "Sheet3 is outside the run");
    }

    #[test]
    fn a_three_d_run_with_a_missing_endpoint_contributes_no_edges() {
        // Half a run is a wrong answer that looks right; #REF! is the honest
        // one, and it is reached by the formula having no edges at all.
        let at = sc(S1, 0, 5);
        let g = wb_graph(&[(at, "=SUM(Sheet1:Nowhere!A1)")]);
        assert!(
            g.precedents_at(at).unwrap().is_empty(),
            "a broken run must not silently cover part of the workbook"
        );
    }

    #[test]
    fn editing_a_sheet_inside_a_three_d_run_recalculates_the_formula() {
        // The dependency-graph half of the acceptance criterion: the MIDDLE
        // sheet of the run is not named in the formula text at all, so only
        // real tab-order resolution can produce this edge.
        let total = sc(S1, 0, 5);
        let g = wb_graph(&[(total, "=SUM(Sheet1:Sheet3!A1)")]);
        for sheet in [S1, S2, S3] {
            assert_eq!(
                g.direct_dependents_at(sc(sheet, 0, 0)),
                vec![total],
                "editing A1 on this sheet must recalculate the 3-D total"
            );
        }
        // And a cell OUTSIDE the referenced rectangle must not.
        assert!(g.direct_dependents_at(sc(S2, 4, 4)).is_empty());
    }

    #[test]
    fn a_cycle_through_a_three_d_run_is_detected() {
        // Sheet2!B1 sums A1:A9 on every sheet of the run; Sheet1!A1 reads
        // Sheet2!B1. That is a genuine two-node loop, and it only exists
        // because the 3-D run really reaches into Sheet1 — with no per-sheet
        // edges there would be no cycle to find.
        let total = sc(S2, 0, 1);
        let feeder = sc(S1, 0, 0);
        let g = wb_graph(&[(total, "=SUM(Sheet1:Sheet3!A1:A9)"), (feeder, "=Sheet2!B1")]);
        assert!(g.is_circular_at(total), "the loop closes through the run");
        assert!(g.is_circular_at(feeder));
        assert!(
            g.full_order_all().is_err(),
            "a cycle must be reported, not sorted into some arbitrary order"
        );

        // The control: move the FEEDER out of the run's rectangle (D1 is not
        // in A1:A9) and the very same shape is no longer a cycle.
        let outside = sc(S2, 50, 1);
        let far = sc(S1, 0, 3);
        let g = wb_graph(&[(outside, "=SUM(Sheet1:Sheet3!A1:A9)"), (far, "=Sheet2!B51")]);
        assert!(!g.is_circular_at(outside));
        assert!(!g.is_circular_at(far));
        assert!(g.full_order_all().is_ok());
    }

    // ---- containment indexing (issue #59) ----

    /// Ordering a range-heavy workbook must not be quadratic in formula count.
    ///
    /// `topo_sort` built edges with `for n in nodes { for m in nodes { ... } }`,
    /// and the two range walks rescanned EVERY formula key per range precedent.
    /// A workbook of thousands of `=SUM(A1:A1000000)` — trivially craftable,
    /// and reachable from any opened file — therefore made recalculation
    /// ordering O(F2), a CPU denial of service on load.
    ///
    /// The assertion is on the GROWTH RATE, not a wall-clock budget: doubling
    /// the formula count must not roughly quadruple the time. A machine-speed
    /// threshold would be flaky on a loaded CI runner, but the shape of the
    /// curve is a property of the algorithm. Measured before the index at
    /// F=2000/4000/8000: 11ms / 38ms / 142ms — ratios of 3.4x and 3.7x,
    /// unmistakably quadratic. After: 2.7 / 5.7 / 10.9ms, ratios 2.1x and 1.9x.
    /// The 3.0x bar sits clearly between the two.
    #[test]
    fn ordering_a_range_heavy_workbook_is_not_quadratic_in_formula_count() {
        fn order_time(f: u32) -> std::time::Duration {
            let mut g = DepGraph::new();
            // Each formula sums a million-row range covering the DATA column,
            // the shape the audit called out. Column 1 holds the formulas,
            // column 0 is the summed data, so this is a wide fan-in with no
            // cycle, and every range walk has every formula to reject.
            let e = parse("=SUM(A1:A1000000)").unwrap();
            for r in 0..f {
                g.set_formula(cr(r, 1), &e);
            }
            let t = std::time::Instant::now();
            let order = g
                .full_order_all()
                .expect("no cycle: formulas read column A");
            let dt = t.elapsed();
            assert_eq!(order.len() as u32, f, "every formula must be ordered");
            dt
        }

        // Warm caches/allocator first so the smallest sample is not inflated,
        // which would flatter the ratio.
        let _ = order_time(1000);

        let small = order_time(4000);
        let large = order_time(8000);

        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
        assert!(
            ratio < 3.0,
            "doubling the formula count multiplied ordering time by {ratio:.1}x \
             ({small:?} -> {large:?}); quadratic growth is ~4x, so the \
             containment index has regressed to a linear rescan"
        );
    }

    /// A tall, narrow range must not cost more than a short one.
    ///
    /// This is the trap a row-major index falls into: `=SUM(A1:A1000000)` is
    /// one column but a million rows, so an index that seeks to the row band
    /// and scans it still touches every formula in those rows whatever their
    /// column — the same full rescan, just reordered. Column-major makes the
    /// walk output-sensitive, so range HEIGHT costs nothing.
    #[test]
    fn a_range_a_million_rows_tall_costs_no_more_than_a_short_one() {
        fn order_time(range: &str) -> std::time::Duration {
            let mut g = DepGraph::new();
            let e = parse(range).unwrap();
            // Formulas spread down column 1, i.e. INSIDE the tall range's row
            // span but outside its column span. A row-major index visits all
            // of them per lookup; a column-major one visits none.
            for r in 0..6000 {
                g.set_formula(cr(r, 1), &e);
            }
            let t = std::time::Instant::now();
            g.full_order_all().expect("no cycle");
            t.elapsed()
        }

        let _ = order_time("=SUM(A1:A10)");
        let short = order_time("=SUM(A1:A10)");
        let tall = order_time("=SUM(A1:A1000000)");

        let ratio = tall.as_secs_f64() / short.as_secs_f64().max(1e-9);
        assert!(
            ratio < 4.0,
            "a range 100,000x taller took {ratio:.1}x as long ({short:?} -> \
             {tall:?}); range height must not enter the cost, or the index is \
             scanning rows instead of columns"
        );
    }

    /// The index must not change WHICH cells are found — only how fast.
    ///
    /// Guards the whole refactor: the old code asked `p.contains(f.cell)`
    /// against every formula key, so the index is only a safe substitution if
    /// it yields exactly that set. Checked against a brute-force rescan over
    /// shapes chosen to hit the edges — a single cell, a one-row and
    /// one-column strip, a rectangle, a range whose corners are reversed
    /// (never normalized, must match nothing), and one that misses entirely.
    #[test]
    fn the_index_finds_exactly_the_cells_a_brute_force_rescan_would() {
        let cells: Vec<SheetCell> = (0..12u32)
            .flat_map(|r| (0..12u32).map(move |c| sc(S1, r * 2, c * 2)))
            .chain([sc(S2, 4, 4), sc(S2, 0, 0)])
            .collect();
        let index = CellIndex::build(cells.iter().copied());

        let shapes = [
            Precedent::Cell(cr(4, 4)),
            Precedent::Cell(cr(5, 5)),
            Precedent::Range(cr(0, 0), cr(0, 22)),
            Precedent::Range(cr(0, 0), cr(22, 0)),
            Precedent::Range(cr(2, 2), cr(10, 8)),
            Precedent::Range(cr(3, 3), cr(9, 7)),
            // Reversed corners: the shared `contains` rejects everything, and
            // the index must agree rather than silently normalizing.
            Precedent::Range(cr(10, 8), cr(2, 2)),
            Precedent::Range(cr(100, 100), cr(200, 200)),
        ];

        for sheet in [S1, S2] {
            for p in &shapes {
                let mut got: Vec<CellRef> = Vec::new();
                index.for_each_in(sheet, p, |c| got.push(c));
                got.sort_by_key(|c| (c.row, c.col));

                let mut want: Vec<CellRef> = cells
                    .iter()
                    .filter(|f| f.sheet == sheet && p.contains(f.cell))
                    .map(|f| f.cell)
                    .collect();
                want.sort_by_key(|c| (c.row, c.col));

                assert_eq!(
                    got, want,
                    "index disagrees with a brute-force rescan for {p:?}"
                );
            }
        }
    }

    /// A partial recalculation must not invent a cycle out of a formula it is
    /// not recalculating.
    ///
    /// `topo_sort` runs on the INDUCED SUBGRAPH — just the affected cells — so
    /// its containment index has to be built over `nodes`, not over every
    /// formula in the workbook. Index the whole graph instead and a range
    /// precedent still matches formulas OUTSIDE the subgraph: the edge is
    /// recorded and the dependent's indegree incremented, but nothing in the
    /// drain will ever decrement it, because the node it came from is not
    /// being sorted. The dependent never reaches indegree 0, and Kahn's
    /// leftover check reports it as STUCK IN A CYCLE.
    ///
    /// So the failure is not a slow recalculation — it is a spurious circular
    /// reference on a workbook with no cycle in it. The performance work is
    /// what introduced the index, so the scoping it depends on gets a test
    /// that fails loudly if it is ever widened.
    #[test]
    fn recalculating_a_subgraph_does_not_report_an_unrelated_formula_as_a_cycle() {
        // A5 is a CONSTANT FORMULA sitting inside the range B1 sums. It is a
        // node in the graph, but it reads nothing, so editing A1 does not
        // affect it — it is in the workbook index and NOT in the subgraph,
        // which is exactly the gap that strands B1's indegree above zero.
        // C1 is deliberately in column 2, OUTSIDE A1:A9, so B1 does not read
        // it back and the workbook genuinely has no cycle.
        let g = graph(&[
            (cr(4, 0), "=99"),
            (cr(0, 1), "=SUM(A1:A9)"),
            (cr(0, 2), "=B1+1"),
        ]);

        let order = g.recalc_order(cr(0, 0)).expect(
            "a formula inside the summed range that is not itself \
                     affected is not a cycle",
        );
        assert_eq!(
            order,
            vec![cr(0, 1), cr(0, 2)],
            "only B1 and C1 are affected, B1 first"
        );

        // The whole-workbook sort still orders the unaffected formula too.
        let full = g.full_order_all().expect("no cycle anywhere in this sheet");
        assert_eq!(full.len(), 3, "all three formulas must be ordered");
    }

    #[test]
    fn sheet_uses_are_recorded_from_the_text_and_found_again() {
        let mut g = DepGraph::new();
        let at = sc(S1, 0, 2);
        // A sheet that does NOT exist: the graph drops the edge, so only the
        // recorded text use can find this formula for a later rename.
        g.set_formula_at(at, &parse("=Nowhere!A1").unwrap(), &names());
        g.set_sheet_uses(at, "=Nowhere!A1");
        assert!(
            g.precedents_at(at).unwrap().is_empty(),
            "an unresolvable sheet contributes no edges"
        );
        assert_eq!(g.sheet_uses_at(at), ["Nowhere"]);
        assert_eq!(g.cells_using_sheet("nowhere"), vec![at]);
        assert!(g.cells_using_sheet("Sheet2").is_empty());
    }

    #[test]
    fn sheet_uses_record_both_endpoints_of_a_run() {
        let mut g = DepGraph::new();
        let at = sc(S1, 0, 0);
        g.set_sheet_uses(at, "=SUM(Sheet1:Sheet3!A1)+\"Sheet2\"");
        assert_eq!(g.sheet_uses_at(at), ["Sheet1", "Sheet3"]);
        assert!(
            g.cells_using_sheet("Sheet2").is_empty(),
            "a sheet named only inside a string literal is not a use"
        );
    }

    #[test]
    fn dropping_a_formula_drops_its_sheet_uses_too() {
        let mut g = DepGraph::new();
        let at = sc(S1, 0, 0);
        g.set_sheet_uses(at, "=Sheet2!A1");
        g.remove_at(at);
        assert!(g.cells_using_sheet("Sheet2").is_empty());

        let other = sc(S1, 5, 5);
        g.set_sheet_uses(other, "=Sheet2!A1");
        g.remove_sheet(S1);
        assert!(g.cells_using_sheet("Sheet2").is_empty());
    }
}
