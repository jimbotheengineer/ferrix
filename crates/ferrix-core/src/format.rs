//! Sheet-wide cell and column formatting, independent of any defined table.
//!
//! [`crate::table`] already knows how to turn a number into a colour: it has
//! [`ConditionalRule`], [`CellStyle`], [`NumberFormat`] and the per-cell
//! resolution the grid calls while painting. What it does *not* have is a way
//! to reach a cell that is not inside a declared [`Table`]. This module is
//! that reach: a [`SheetFormat`] hangs off a sheet rather than a table, and
//! answers "how should this cell look" for any cell in it.
//!
//! [`Table`]: crate::Table
//!
//! ## The storage problem, and the shape of the answer
//!
//! The obvious implementation of "colour this cell" is a `HashMap<CellRef,
//! Style>`. It is also unusable here. Ferrix targets 200M-row files; a map
//! entry is ~48 bytes once hashing overhead is counted, so coloring one whole
//! column costs about **9.6 GB** — to express a fact that fits in six bytes,
//! because the answer is the same for every row.
//!
//! So nothing in this module is stored per row. There are exactly three
//! places formatting can live, and they are consulted in this order (later
//! wins, matching Excel's own rule precedence):
//!
//! 1. **Column scope** — [`ColumnFormat`], one entry per *formatted column*.
//!    A rule here applies to every row of that column, including rows that do
//!    not exist yet. This is what makes "negative numbers red" cost 24 bytes
//!    instead of gigabytes, and what makes it keep working after a paste
//!    appends ten million rows.
//! 2. **Range scope** — [`RangeFormat`], one entry per user-selected
//!    rectangle. Storing the rectangle, not its cells: `B2:B50000000` is one
//!    entry.
//! 3. **Cell overrides** — [`CellOverride`], a genuinely sparse map. This is
//!    the [`EditOverlay`]-shaped case: the user has picked out *individual*
//!    cells, so there are tens of them, not millions. It is the only per-cell
//!    storage in the module and it is opt-in — nothing writes to it except an
//!    explicit single-cell command.
//!
//! [`EditOverlay`]: crate::EditOverlay
//!
//! Consequently [`SheetFormat::heap_bytes`] is a function of *how many rules
//! the user configured*, and is completely independent of how many rows they
//! apply to. `tests/format_scale.rs` asserts exactly that, and also that
//! resolving a viewport of cells performs zero heap allocations regardless of
//! whether the column is 200 rows or 200,000,000.
//!
//! ## Evaluation is lazy, per painted cell
//!
//! [`SheetFormat::resolve`] is called for the ~1,500 cells actually on screen
//! and touches nothing else. It takes a value the caller already had, walks a
//! plan of at most a handful of rules, and returns a [`CellStyle`] by value.
//! It allocates nothing.
//!
//! Two rule kinds need to know something about the column as a whole — colour
//! scales and data bars need its range, top/bottom-N needs a rank cut. Those
//! cannot be answered from one cell, and answering them exactly would mean
//! scanning 200M rows on every frame. Ferrix does what it already does for
//! table colour scales: it computes them over the **visible window** and says
//! so. See [`RuleEval`].
//!
//! ## Building the plan
//!
//! Because rules live in three scopes, the ordered list of rules affecting a
//! given column is assembled by [`SheetFormat::plan`], once per visible
//! column per frame (~30 calls), into a caller-owned buffer. Per-cell work is
//! then a walk over that slice with an integer row-bounds check — no map
//! lookup, no allocation.

use std::collections::BTreeMap;

use crate::sheet::CellRef;
use crate::table::{CellStyle, CmpOp, ConditionalRule, NumberFormat, Rgb, TableRange};
use crate::value::Value;

// ==================================================================== rules ==

/// Everything a rule needs to know about its column that one cell cannot say.
///
/// Both fields are computed over the **visible rows only**. That is a
/// deliberate, documented approximation, and it is the same one
/// `TableDecor::prepare` already makes for table colour scales: an exact
/// answer would require scanning every row of the column on every repaint,
/// which at 200M rows is seconds per frame. Scrolling therefore rescales the
/// gradient, which is the behaviour a user can see and reason about — as
/// opposed to a frozen frame, which is not.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct RuleEval {
    /// `(min, max)` of the numeric values in the window. Colour scales and
    /// data bars no-op without it.
    pub extent: Option<(f64, f64)>,
    /// Rank cut for [`ConditionalRule::TopBottom`]: the value at the Nth
    /// position from the requested end, within the window.
    pub cut: Option<f64>,
}

impl RuleEval {
    /// Compute the evaluation context a rule needs from a window of values.
    ///
    /// `values` is the numeric content of the visible slice of the column.
    /// Rules that need neither extent nor rank return an empty context
    /// without looking at the slice at all, so the caller can skip collecting
    /// it entirely — see [`ConditionalRule::needs_window`].
    pub fn for_rule(rule: &ConditionalRule, values: &mut [f64]) -> Self {
        let mut out = RuleEval::default();
        if values.is_empty() {
            return out;
        }
        if rule.needs_extent() {
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for &v in values.iter() {
                lo = lo.min(v);
                hi = hi.max(v);
            }
            out.extent = Some((lo, hi));
        }
        if let ConditionalRule::TopBottom { top, n, .. } = rule {
            let n = (*n as usize).clamp(1, values.len());
            // `select_nth_unstable` is O(len) and needs no allocation, which
            // matters because this runs once per rule per frame.
            let idx = if *top { values.len() - n } else { n - 1 };
            let (_, cut, _) = values.select_nth_unstable_by(idx, f64::total_cmp);
            out.cut = Some(*cut);
        }
        out
    }
}

/// One entry of a column's resolved rule plan.
///
/// Produced by [`SheetFormat::plan`] and consumed per painted cell. `rows` is
/// `None` for a column-scoped rule and `Some((first, last))` for a
/// range-scoped one, so restricting a rule to its rows is two integer
/// comparisons rather than a lookup.
#[derive(Clone, Copy, Debug)]
pub struct PlanEntry<'a> {
    pub rule: &'a ConditionalRule,
    pub rows: Option<(u32, u32)>,
}

impl PlanEntry<'_> {
    #[inline]
    pub fn covers(&self, row: u32) -> bool {
        match self.rows {
            None => true,
            Some((a, b)) => row >= a && row <= b,
        }
    }
}

// ============================================================ manual styles ==

/// A colour the user picked by hand, with no condition attached.
///
/// Modelled as `Option` per channel rather than a `CellStyle` so that setting
/// only a fill leaves a lower-precedence text colour alone, which is what
/// "set the background" means in every spreadsheet.
/// Type styling for a cell: family, size, and the weight/slant/underline
/// switches every spreadsheet is expected to have.
///
/// Every field is optional and `None` means "inherit". That is what lets a
/// column-level rule set the family while a single cell overrides only the
/// weight, without either having to know about the other — and it is why this
/// is a handful of bytes rather than a resolved font per cell. At 200M rows a
/// resolved-per-cell representation would be tens of gigabytes for something
/// that is almost always uniform.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Typography {
    pub family: Option<FontFamily>,
    /// Points. `None` inherits the grid's default.
    pub size: Option<f32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strikethrough: Option<bool>,
}

/// The font families Ferrix ships.
///
/// Deliberately a small closed set rather than an arbitrary string: a font
/// name that is not installed renders as something else entirely, and a
/// spreadsheet that silently changes appearance between machines is worse
/// than one with fewer choices. `Monospace` matters for data: digits line up
/// in columns, which is most of the reason to look at a spreadsheet at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FontFamily {
    #[default]
    Proportional,
    Monospace,
}

impl Typography {
    pub fn is_empty(&self) -> bool {
        self.family.is_none()
            && self.size.is_none()
            && self.bold.is_none()
            && self.italic.is_none()
            && self.underline.is_none()
            && self.strikethrough.is_none()
    }

    /// Layer `self` over `out`: set fields win, `None` fields leave `out`
    /// alone. Applied outermost-scope-first, so a cell override beats a
    /// column default.
    #[inline]
    pub fn apply_to(&self, out: &mut Typography) {
        if self.family.is_some() {
            out.family = self.family;
        }
        if self.size.is_some() {
            out.size = self.size;
        }
        if self.bold.is_some() {
            out.bold = self.bold;
        }
        if self.italic.is_some() {
            out.italic = self.italic;
        }
        if self.underline.is_some() {
            out.underline = self.underline;
        }
        if self.strikethrough.is_some() {
            out.strikethrough = self.strikethrough;
        }
    }

    /// Resolved values, for a renderer that needs concrete answers.
    pub fn resolved(&self, default_size: f32) -> ResolvedType {
        ResolvedType {
            family: self.family.unwrap_or_default(),
            size: self.size.unwrap_or(default_size),
            bold: self.bold.unwrap_or(false),
            italic: self.italic.unwrap_or(false),
            underline: self.underline.unwrap_or(false),
            strikethrough: self.strikethrough.unwrap_or(false),
        }
    }

    /// Flip one switch, used by the toolbar toggles.
    pub fn with_bold(mut self, on: bool) -> Self {
        self.bold = Some(on);
        self
    }
    pub fn with_italic(mut self, on: bool) -> Self {
        self.italic = Some(on);
        self
    }
    pub fn with_underline(mut self, on: bool) -> Self {
        self.underline = Some(on);
        self
    }
    pub fn with_size(mut self, pt: f32) -> Self {
        self.size = Some(pt);
        self
    }
    pub fn with_family(mut self, f: FontFamily) -> Self {
        self.family = Some(f);
        self
    }
}

/// A fully resolved type style — no inheritance left to do.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ResolvedType {
    pub family: FontFamily,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

/// Smallest and largest point size the UI offers.
///
/// Bounded because the row height is fixed: a 200pt font in a 22px row draws
/// over its neighbours, and a 1pt font is unreadable. Clamping here means no
/// caller can produce an unrenderable sheet.
pub const MIN_FONT_PT: f32 = 6.0;
pub const MAX_FONT_PT: f32 = 72.0;

/// Clamp a requested point size into the renderable range.
pub fn clamp_font_pt(pt: f32) -> f32 {
    pt.clamp(MIN_FONT_PT, MAX_FONT_PT)
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct ManualStyle {
    pub fill: Option<Rgb>,
    pub text: Option<Rgb>,
    /// Font family, size, and the bold/italic/underline switches.
    pub typography: Typography,
}

impl ManualStyle {
    pub fn is_empty(&self) -> bool {
        self.fill.is_none() && self.text.is_none() && self.typography.is_empty()
    }

    #[inline]
    pub fn apply_to(&self, out: &mut CellStyle) {
        if let Some(f) = self.fill {
            out.fill = Some(f);
        }
        if let Some(t) = self.text {
            out.text = Some(t);
        }
        self.typography.apply_to(&mut out.typography);
    }
}

// =================================================================== scopes ==

/// Formatting that applies to an entire column, present and future.
///
/// One of these is ~40 bytes plus its rules. It says nothing about how many
/// rows the column has, which is the entire point: appending rows costs
/// nothing and changes nothing here.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ColumnFormat {
    /// How numbers in this column are rendered.
    pub format: NumberFormat,
    /// Rules, evaluated in order. Later entries override earlier ones.
    pub rules: Vec<ConditionalRule>,
}

impl ColumnFormat {
    pub fn is_empty(&self) -> bool {
        self.format == NumberFormat::General && self.rules.is_empty()
    }

    fn heap_bytes(&self) -> usize {
        rules_heap_bytes(&self.rules) + format_heap_bytes(&self.format)
    }
}

/// Formatting that applies to an explicit rectangle.
///
/// The rectangle is stored, not its cells. `A1:XFD50000000` is the same 60-odd
/// bytes as `A1:B2`.
#[derive(Clone, PartialEq, Debug)]
pub struct RangeFormat {
    pub range: TableRange,
    /// Overrides the column format inside this rectangle when set.
    pub format: Option<NumberFormat>,
    pub rules: Vec<ConditionalRule>,
}

impl RangeFormat {
    pub fn new(range: TableRange) -> Self {
        Self {
            range,
            format: None,
            rules: Vec::new(),
        }
    }

    pub fn with_rule(mut self, rule: ConditionalRule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.format.is_none() && self.rules.is_empty()
    }

    fn heap_bytes(&self) -> usize {
        rules_heap_bytes(&self.rules)
            + self.format.as_ref().map_or(0, format_heap_bytes)
            + std::mem::size_of::<RangeFormat>()
    }
}

/// A hand-picked colour on one specific cell.
///
/// The only per-cell storage in this module. It exists because "make *this*
/// cell yellow" is a real thing users do and a column rule cannot express it —
/// but it is reached only by an explicit single-cell command, so the map stays
/// in the tens of entries rather than the millions. Anything that would touch
/// a whole column or selection goes to [`ColumnFormat`] or [`RangeFormat`]
/// instead.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct CellOverride {
    pub manual: ManualStyle,
    pub format: Option<NumberFormat>,
}

impl CellOverride {
    pub fn is_empty(&self) -> bool {
        self.manual.is_empty() && self.format.is_none()
    }
}

fn format_heap_bytes(f: &NumberFormat) -> usize {
    match f {
        NumberFormat::Currency { symbol, .. } => symbol.capacity(),
        NumberFormat::Custom(s) => s.capacity(),
        _ => 0,
    }
}

fn rules_heap_bytes(rules: &[ConditionalRule]) -> usize {
    let mut n = std::mem::size_of_val(rules);
    for r in rules {
        if let ConditionalRule::TextContains { needle, .. } = r {
            n += needle.capacity();
        }
    }
    n
}

// ============================================================== sheet format ==

/// All formatting attached to one sheet.
///
/// See the module docs for the storage argument. In short: two maps keyed by
/// *scope*, never by cell, plus one deliberately sparse per-cell map.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SheetFormat {
    /// Keyed by sheet column. `BTreeMap` rather than `HashMap` so saving is
    /// byte-reproducible without a sort, the same reason `edits.rs` sorts its
    /// cells before writing.
    columns: BTreeMap<u32, ColumnFormat>,
    /// In user-visible order; later entries override earlier ones.
    ranges: Vec<RangeFormat>,
    /// Sparse, explicit, opt-in. See [`CellOverride`].
    overrides: BTreeMap<(u32, u32), CellOverride>,
}

impl SheetFormat {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.ranges.is_empty() && self.overrides.is_empty()
    }

    /// Bytes of heap this store owns.
    ///
    /// Deliberately public: it is the number the scale test asserts on. It
    /// counts rules, ranges, overrides and interned format strings, and it is
    /// a function of how many rules exist — never of how many rows they cover.
    pub fn heap_bytes(&self) -> usize {
        let cols: usize = self
            .columns
            .values()
            .map(|c| c.heap_bytes() + std::mem::size_of::<(u32, ColumnFormat)>())
            .sum();
        let ranges: usize = self.ranges.iter().map(|r| r.heap_bytes()).sum();
        let ov = self.overrides.len() * std::mem::size_of::<((u32, u32), CellOverride)>();
        cols + ranges + ov
    }

    /// How many rules are configured, across every scope.
    pub fn rule_count(&self) -> usize {
        self.columns.values().map(|c| c.rules.len()).sum::<usize>()
            + self.ranges.iter().map(|r| r.rules.len()).sum::<usize>()
    }

    pub fn override_count(&self) -> usize {
        self.overrides.len()
    }

    // --- column scope ---

    pub fn column(&self, col: u32) -> Option<&ColumnFormat> {
        self.columns.get(&col)
    }

    pub fn column_mut(&mut self, col: u32) -> &mut ColumnFormat {
        self.columns.entry(col).or_default()
    }

    pub fn columns(&self) -> impl Iterator<Item = (u32, &ColumnFormat)> {
        self.columns.iter().map(|(c, f)| (*c, f))
    }

    /// Set a column's number format. `General` removes it.
    pub fn set_column_format(&mut self, col: u32, format: NumberFormat) {
        if format == NumberFormat::General {
            if let Some(c) = self.columns.get_mut(&col) {
                c.format = NumberFormat::General;
            }
        } else {
            self.column_mut(col).format = format;
        }
        self.prune();
    }

    /// Paint a whole column a fixed colour.
    ///
    /// Stored as a single unconditional [`ConditionalRule::Manual`] on the
    /// column — 24 bytes, whatever the row count. Passing two `None`s clears
    /// any existing manual rule instead of adding an inert one.
    pub fn set_column_manual(&mut self, col: u32, manual: ManualStyle) {
        let entry = self.column_mut(col);
        entry
            .rules
            .retain(|r| !matches!(r, ConditionalRule::Manual { .. }));
        if !manual.is_empty() {
            // Manual colour goes first so a value-driven rule configured later
            // still wins on the cells it matches; that is what makes "red if
            // negative" visible on a column the user also tinted.
            entry.rules.insert(
                0,
                ConditionalRule::Manual {
                    fill: manual.fill,
                    text: manual.text,
                    typography: manual.typography,
                },
            );
        }
        self.prune();
    }

    /// Append a rule to a column. Returns its index within that column.
    pub fn push_column_rule(&mut self, col: u32, rule: ConditionalRule) -> usize {
        let c = self.column_mut(col);
        c.rules.push(rule);
        c.rules.len() - 1
    }

    /// The rules on a column, in precedence order (later wins).
    ///
    /// Borrowed and empty-by-default so the editor can list a column that has
    /// never been formatted without materialising an entry for it — asking
    /// what a column looks like must not be what creates it.
    pub fn column_rules(&self, col: u32) -> &[ConditionalRule] {
        self.columns.get(&col).map_or(&[], |c| &c.rules)
    }

    /// Replace one column rule in place, keeping its position in the order.
    ///
    /// Distinct from remove-then-push because editing a rule must not silently
    /// promote it to winning over everything else that was already there.
    pub fn set_column_rule(&mut self, col: u32, i: usize, rule: ConditionalRule) -> bool {
        let Some(c) = self.columns.get_mut(&col) else {
            return false;
        };
        match c.rules.get_mut(i) {
            Some(slot) => {
                *slot = rule;
                true
            }
            None => false,
        }
    }

    // --- range scope ---

    pub fn ranges(&self) -> &[RangeFormat] {
        &self.ranges
    }

    pub fn range_mut(&mut self, i: usize) -> Option<&mut RangeFormat> {
        self.ranges.get_mut(i)
    }

    pub fn push_range(&mut self, rf: RangeFormat) -> usize {
        self.ranges.push(rf);
        self.ranges.len() - 1
    }

    /// Colour an arbitrary rectangle — the "format this selection" command.
    ///
    /// A 200M-cell selection and a 4-cell one produce the same one entry.
    pub fn set_range_manual(&mut self, range: TableRange, manual: ManualStyle) -> usize {
        self.push_range(RangeFormat::new(range).with_rule(ConditionalRule::Manual {
            fill: manual.fill,
            text: manual.text,
            typography: manual.typography,
        }))
    }

    pub fn remove_range(&mut self, i: usize) -> Option<RangeFormat> {
        (i < self.ranges.len()).then(|| self.ranges.remove(i))
    }

    /// Index of the range entry covering exactly `range`, if one exists.
    ///
    /// Exact match rather than overlap: the editor's unit of work is "the
    /// rules on THIS selection", and two different selections that happen to
    /// intersect are two different lists. Matching by overlap would let a rule
    /// added to `B2:B9` silently appear in the list for `A1:C3`.
    pub fn range_index_of(&self, range: TableRange) -> Option<usize> {
        self.ranges.iter().position(|r| r.range == range)
    }

    /// The rules on an exact range, in precedence order.
    pub fn rules_for_range(&self, range: TableRange) -> &[ConditionalRule] {
        self.range_index_of(range)
            .map_or(&[], |i| &self.ranges[i].rules)
    }

    /// Append a rule to the entry for `range`, creating the entry if needed.
    ///
    /// Returns `(range index, rule index)`. ONE entry however many cells the
    /// rectangle spans — a rule over `B2:B100000000` costs exactly what one
    /// over `B2:B3` costs.
    pub fn push_rule_for_range(
        &mut self,
        range: TableRange,
        rule: ConditionalRule,
    ) -> (usize, usize) {
        let ri = match self.range_index_of(range) {
            Some(i) => i,
            None => self.push_range(RangeFormat::new(range)),
        };
        let rules = &mut self.ranges[ri].rules;
        rules.push(rule);
        (ri, rules.len() - 1)
    }

    /// Replace one range rule in place, keeping its position in the order.
    pub fn set_range_rule(&mut self, range_index: usize, i: usize, rule: ConditionalRule) -> bool {
        let Some(r) = self.ranges.get_mut(range_index) else {
            return false;
        };
        match r.rules.get_mut(i) {
            Some(slot) => {
                *slot = rule;
                true
            }
            None => false,
        }
    }

    // --- per-cell overrides ---

    pub fn cell_override(&self, cell: CellRef) -> Option<&CellOverride> {
        self.overrides.get(&(cell.row, cell.col))
    }

    /// Set a single cell's hand-picked colour. Clearing removes the entry, so
    /// the map never accumulates inert rows.
    pub fn set_cell_override(&mut self, cell: CellRef, ov: CellOverride) {
        if ov.is_empty() {
            self.overrides.remove(&(cell.row, cell.col));
        } else {
            self.overrides.insert((cell.row, cell.col), ov);
        }
    }

    pub fn overrides(&self) -> impl Iterator<Item = (CellRef, &CellOverride)> {
        self.overrides
            .iter()
            .map(|((r, c), o)| (CellRef::new(*r, *c), o))
    }

    /// Drop entries that no longer say anything, so an emptied column does not
    /// keep a live key and inflate `heap_bytes`.
    fn prune(&mut self) {
        self.columns.retain(|_, c| !c.is_empty());
        self.ranges.retain(|r| !r.is_empty());
    }

    // --- reordering ---

    /// Move a column rule one place later (towards winning) or earlier.
    ///
    /// Order is user-visible because it is user-meaningful: with "red if < 0"
    /// and "green if < -100" both configured, which one you see on -500 is
    /// entirely a question of which is last. The editor therefore exposes this
    /// rather than picking for the user.
    pub fn move_column_rule(&mut self, col: u32, i: usize, delta: isize) -> bool {
        let Some(c) = self.columns.get_mut(&col) else {
            return false;
        };
        move_within(&mut c.rules, i, delta)
    }

    pub fn move_range_rule(&mut self, range_index: usize, i: usize, delta: isize) -> bool {
        let Some(r) = self.ranges.get_mut(range_index) else {
            return false;
        };
        move_within(&mut r.rules, i, delta)
    }

    /// Move a whole range entry in the precedence order.
    pub fn move_range(&mut self, i: usize, delta: isize) -> bool {
        move_within(&mut self.ranges, i, delta)
    }

    pub fn remove_column_rule(&mut self, col: u32, i: usize) -> Option<ConditionalRule> {
        let c = self.columns.get_mut(&col)?;
        if i >= c.rules.len() {
            return None;
        }
        let r = c.rules.remove(i);
        self.prune();
        Some(r)
    }

    pub fn remove_range_rule(&mut self, range_index: usize, i: usize) -> Option<ConditionalRule> {
        let r = self.ranges.get_mut(range_index)?;
        if i >= r.rules.len() {
            return None;
        }
        let out = r.rules.remove(i);
        self.prune();
        Some(out)
    }

    // --- resolution ---

    /// Collect the ordered rules affecting `col` into `out`.
    ///
    /// Called once per visible column per frame, not per cell. `out` is
    /// caller-owned and reused, so a steady-state repaint allocates nothing
    /// here either.
    pub fn plan<'a>(&'a self, col: u32, out: &mut Vec<PlanEntry<'a>>) {
        out.clear();
        if let Some(c) = self.columns.get(&col) {
            for rule in &c.rules {
                out.push(PlanEntry { rule, rows: None });
            }
        }
        for rf in &self.ranges {
            if col < rf.range.first_col || col > rf.range.last_col {
                continue;
            }
            for rule in &rf.rules {
                out.push(PlanEntry {
                    rule,
                    rows: Some((rf.range.first_row, rf.range.last_row)),
                });
            }
        }
    }

    /// Does any rule in `plan` need the cell's display text?
    ///
    /// Resolving display text allocates a `String`, so the caller asks first
    /// and pays only when a text rule is actually configured. This is the same
    /// bargain `TableDecor::cell` strikes for validation text.
    pub fn plan_needs_text(plan: &[PlanEntry<'_>]) -> bool {
        plan.iter().any(|e| e.rule.needs_text())
    }

    /// Does any rule in `plan` need a window scan?
    pub fn plan_needs_window(plan: &[PlanEntry<'_>]) -> bool {
        plan.iter().any(|e| e.rule.needs_window())
    }

    /// The number format for a cell: range scope wins over column scope, and
    /// an explicit per-cell override wins over both.
    ///
    /// Returns a borrow — resolving a format must not clone a currency symbol
    /// 1,500 times per frame.
    pub fn number_format(&self, cell: CellRef) -> Option<&NumberFormat> {
        if let Some(f) = self
            .overrides
            .get(&(cell.row, cell.col))
            .and_then(|o| o.format.as_ref())
        {
            return Some(f);
        }
        let mut out = self.columns.get(&cell.col).map(|c| &c.format);
        for rf in &self.ranges {
            if rf.range.contains(cell) {
                if let Some(f) = &rf.format {
                    out = Some(f);
                }
            }
        }
        out.filter(|f| **f != NumberFormat::General)
    }

    /// Resolve one cell's style.
    ///
    /// **Allocates nothing.** `plan` and `evals` are prepared per column per
    /// frame; this walks them, applies whatever matches, then lets an explicit
    /// per-cell override have the last word. `text` may be empty when
    /// [`SheetFormat::plan_needs_text`] said no rule needed it.
    ///
    /// `evals` is indexed in lockstep with `plan`; a short slice simply means
    /// the window-dependent rules no-op, which is the correct degradation when
    /// the caller chose not to scan.
    pub fn resolve(
        &self,
        cell: CellRef,
        value: &Value,
        text: &str,
        plan: &[PlanEntry<'_>],
        evals: &[RuleEval],
    ) -> CellStyle {
        let mut style = CellStyle::default();
        for (i, entry) in plan.iter().enumerate() {
            if !entry.covers(cell.row) {
                continue;
            }
            let eval = evals.get(i).copied().unwrap_or_default();
            entry.rule.apply_cell(value, text, eval, &mut style);
        }
        if let Some(o) = self.overrides.get(&(cell.row, cell.col)) {
            o.manual.apply_to(&mut style);
        }
        style
    }
}

/// Shift `v[i]` by `delta`, clamped. Returns whether anything moved.
fn move_within<T>(v: &mut [T], i: usize, delta: isize) -> bool {
    if i >= v.len() || delta == 0 {
        return false;
    }
    let target = (i as isize + delta).clamp(0, v.len() as isize - 1) as usize;
    if target == i {
        return false;
    }
    if target > i {
        v[i..=target].rotate_left(1);
    } else {
        v[target..=i].rotate_right(1);
    }
    true
}

// ================================================== rule evaluation helpers ==

/// Preset rules the editor offers as one-click starting points.
///
/// These are not a separate rule kind — each is just a [`ConditionalRule`]
/// with sensible colours pre-filled, so nothing downstream (evaluation,
/// persistence, xlsx export) needs to know a preset existed.
pub mod presets {
    use super::*;

    /// Excel's negative-red / positive-green convention, as one rule.
    pub fn sign_colors() -> ConditionalRule {
        ConditionalRule::Sign {
            negative: Some(Rgb(0xC0, 0x28, 0x28)),
            positive: Some(Rgb(0x1E, 0x88, 0x3C)),
            zero: None,
        }
    }

    /// Negative numbers red, positives left alone.
    pub fn negative_red() -> ConditionalRule {
        ConditionalRule::Sign {
            negative: Some(Rgb(0xC0, 0x28, 0x28)),
            positive: None,
            zero: None,
        }
    }

    /// `> value` highlighted green, Excel's "good" palette.
    pub fn above(value: f64) -> ConditionalRule {
        ConditionalRule::Threshold {
            op: CmpOp::Gt,
            value,
            fill: Rgb(0xC6, 0xEF, 0xCE),
            text: Rgb(0x00, 0x61, 0x00),
        }
    }

    /// `< value` highlighted red, Excel's "bad" palette.
    pub fn below(value: f64) -> ConditionalRule {
        ConditionalRule::Threshold {
            op: CmpOp::Lt,
            value,
            fill: Rgb(0xFF, 0xC7, 0xCE),
            text: Rgb(0x9C, 0x00, 0x06),
        }
    }

    pub fn top_n(n: u32) -> ConditionalRule {
        ConditionalRule::TopBottom {
            top: true,
            n,
            fill: Rgb(0xC6, 0xEF, 0xCE),
            text: Rgb(0x00, 0x61, 0x00),
        }
    }

    pub fn bottom_n(n: u32) -> ConditionalRule {
        ConditionalRule::TopBottom {
            top: false,
            n,
            fill: Rgb(0xFF, 0xC7, 0xCE),
            text: Rgb(0x9C, 0x00, 0x06),
        }
    }

    pub fn contains(needle: impl Into<String>) -> ConditionalRule {
        ConditionalRule::TextContains {
            needle: needle.into(),
            fill: Rgb(0xFF, 0xEB, 0x9C),
            text: Rgb(0x9C, 0x65, 0x00),
        }
    }

    /// The white-to-blue scale Ferrix uses as its default gradient.
    pub fn color_scale() -> ConditionalRule {
        ConditionalRule::ColorScale2 {
            min: Rgb(0xFF, 0xFF, 0xFF),
            max: Rgb(0x53, 0x8D, 0xD5),
        }
    }

    pub fn data_bar() -> ConditionalRule {
        ConditionalRule::DataBar {
            color: Rgb(0x63, 0x8E, 0xC6),
        }
    }
}

#[cfg(test)]
mod tests;
