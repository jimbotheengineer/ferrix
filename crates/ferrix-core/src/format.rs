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

/// One entry of a column's resolved *decoration* plan (issue #28).
///
/// The decoration twin of [`PlanEntry`], and built the same way and for the
/// same reason: [`SheetFormat::decor_plan`] assembles the ordered list of
/// decorations affecting one column ONCE per visible column per frame, and
/// the per-cell work is then a walk over that slice with two integer
/// comparisons. `decor` is `Copy` and heap-free, so the entry is carried by
/// value rather than borrowed.
#[derive(Clone, Copy, Debug)]
pub struct DecorEntry {
    pub decor: CellDecor,
    /// `None` for column scope, `Some((first, last))` for range scope.
    pub rows: Option<(u32, u32)>,
}

impl DecorEntry {
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

// ============================================================ cell decoration ==
//
// Issue #28. Everything below follows the [`Typography`] pattern exactly:
// every field is `Option`, `None` means "inherit", and the whole struct is
// `Copy` with no heap of its own. That is what lets a column-scope decoration
// and a range-scope decoration layer without either knowing about the other,
// and it is why decorating 10,000,000 rows costs one entry rather than ten
// million.

/// How a border edge is drawn. `None` is the absence of a border and is
/// spelled by `Option<Border>` being `None`, not by a variant here — so
/// "inherit" and "explicitly no border" stay distinguishable, which matters
/// when a range clears a border the column set.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BorderStyle {
    /// Explicitly no line. Distinct from inheriting: this ERASES a lower
    /// scope's edge rather than deferring to it.
    #[default]
    None,
    Thin,
    Medium,
    Thick,
    Double,
    Dotted,
    Dashed,
}

impl BorderStyle {
    /// Pixel width the renderer draws this at, before zoom.
    ///
    /// `Double` reports the width of ONE of its two lines; the painter draws
    /// two of them, which is why it is not simply "thick".
    pub fn width(self) -> f32 {
        match self {
            BorderStyle::None => 0.0,
            BorderStyle::Thin | BorderStyle::Dotted | BorderStyle::Dashed => 1.0,
            BorderStyle::Medium | BorderStyle::Double => 1.6,
            BorderStyle::Thick => 2.6,
        }
    }

    /// Does this style draw anything at all?
    pub fn is_visible(self) -> bool {
        self != BorderStyle::None
    }

    /// The `<border>` child element's `style` attribute in OOXML.
    pub fn ooxml(self) -> &'static str {
        match self {
            BorderStyle::None => "none",
            BorderStyle::Thin => "thin",
            BorderStyle::Medium => "medium",
            BorderStyle::Thick => "thick",
            BorderStyle::Double => "double",
            BorderStyle::Dotted => "dotted",
            BorderStyle::Dashed => "dashed",
        }
    }

    /// Every style, for exhaustive round-trip tests.
    pub const ALL: [BorderStyle; 7] = [
        BorderStyle::None,
        BorderStyle::Thin,
        BorderStyle::Medium,
        BorderStyle::Thick,
        BorderStyle::Double,
        BorderStyle::Dotted,
        BorderStyle::Dashed,
    ];
}

/// One border edge: a style and its colour.
///
/// The colour is `Option` for the same reason everything else here is —
/// `None` means "the theme's grid ink", so a border set before a theme
/// switch does not pin itself to a colour that becomes invisible.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Border {
    pub style: BorderStyle,
    pub color: Option<Rgb>,
}

impl Border {
    pub const fn new(style: BorderStyle) -> Self {
        Self { style, color: None }
    }

    pub const fn colored(style: BorderStyle, color: Rgb) -> Self {
        Self {
            style,
            color: Some(color),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.style.is_visible()
    }
}

/// Which side of a cell an edge is on.
///
/// Ordered so the array index is the discriminant and `Side::ALL` can be
/// iterated without a match.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Left = 0,
    Right = 1,
    Top = 2,
    Bottom = 3,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Which way a diagonal runs. Excel models these as two independent flags on
/// one border, and so does this.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Diagonal {
    #[default]
    /// Bottom-left to top-right.
    Up,
    /// Top-left to bottom-right.
    Down,
    /// Both, forming an X.
    Both,
}

impl Diagonal {
    pub fn up(self) -> bool {
        matches!(self, Diagonal::Up | Diagonal::Both)
    }
    pub fn down(self) -> bool {
        matches!(self, Diagonal::Down | Diagonal::Both)
    }
}

/// Horizontal text placement inside a cell.
///
/// `General` is not "left": it is the type-driven default the grid already
/// applies — numbers right, text left, booleans centred. Keeping it as a
/// distinct variant means a user can explicitly ask for that behaviour back
/// after setting an alignment, which "unset" alone cannot express once the
/// value has been persisted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum HAlign {
    #[default]
    General,
    Left,
    Center,
    Right,
    /// Excel's `justify`. Ferrix renders it as left; recorded so the round
    /// trip does not silently rewrite the user's choice.
    Justify,
}

/// Vertical text placement inside a cell. Only observable once the row is
/// taller than one line — which wrapping and rotation both make happen.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum VAlign {
    Top,
    #[default]
    Center,
    Bottom,
}

/// Largest indent level, matching Excel. One level is [`INDENT_STEP_PX`].
pub const MAX_INDENT: u8 = 15;
/// Pixels one indent level adds, before zoom. Excel uses three character
/// widths; this is the equivalent at the grid's default font.
pub const INDENT_STEP_PX: f32 = 9.0;

/// Rotation limits, in degrees. Positive is counter-clockwise, matching both
/// Excel's dialog and the sign convention of the OOXML attribute for the
/// 0..=90 half.
pub const MIN_ROTATION: i16 = -90;
pub const MAX_ROTATION: i16 = 90;

/// Clamp a requested rotation into the representable range.
pub fn clamp_rotation(deg: i16) -> i16 {
    deg.clamp(MIN_ROTATION, MAX_ROTATION)
}

/// Clamp a requested indent level.
pub fn clamp_indent(level: u8) -> u8 {
    level.min(MAX_INDENT)
}

// -------------------------------------------------------- wrapped-row height ==

/// Average glyph advance the wrap estimator assumes, in unzoomed pixels.
///
/// The same 7.2 the autofit estimator in the UI already uses, kept here so
/// paint, hit-testing and the editor all derive a wrapped row's height from
/// ONE arithmetic definition. That is the whole point of putting it in the
/// model instead of measuring a galley in the paint loop: a galley is only
/// available where an egui `Fonts` is, and the hit test and
/// `cell_screen_rect` run where one is not. Two measurements would agree
/// until a font changed and then hit-test a different row than was painted.
pub const WRAP_CHAR_PX: f32 = 7.2;

/// Horizontal padding a cell reserves for its text, both edges together.
pub const CELL_TEXT_PAD_PX: f32 = 12.0;

/// How many lines `text` needs when wrapped into `width_px`.
///
/// Counts explicit newlines too, so a multi-line string reports its real
/// line count rather than one long run. Always at least 1: an empty cell
/// still occupies a line.
///
/// This is an ESTIMATE, and deliberately so — see [`WRAP_CHAR_PX`]. It is
/// monotonic in text length, which is the property the feature needs: more
/// text never produces a shorter row.
pub fn wrapped_line_count(text: &str, width_px: f32, indent_px: f32) -> u32 {
    let usable = width_px - CELL_TEXT_PAD_PX - indent_px;
    // A column narrower than one glyph would divide toward infinity lines.
    // One character per line is the floor, which is also what a real
    // renderer does when it cannot break any smaller.
    let per_line = (usable / WRAP_CHAR_PX).floor().max(1.0) as usize;
    let mut lines = 0u32;
    for segment in text.split('\n') {
        let chars = segment.chars().count().max(1);
        lines += chars.div_ceil(per_line) as u32;
    }
    lines.max(1)
}

/// Largest number of lines a wrapped row is allowed to grow to.
///
/// Bounded because an unbounded row height is a row that fills the viewport
/// and hides every other row — and because the scroll model measures its
/// extent in ROWS, so one enormous row degrades scrolling for the whole
/// sheet.
pub const MAX_WRAP_LINES: u32 = 12;

/// Largest number of columns the wrapped-row-height scan will consider.
///
/// See [`SheetFormat::wrapping_cols`]. This bounds the per-row cost of the
/// feature so a wrap applied to an enormous range stays a viewport-sized
/// amount of work rather than a sheet-sized one.
pub const WRAP_COL_SCAN_CAP: usize = 64;

/// Height a row needs to show `lines` wrapped lines, given the sheet default.
///
/// One line is exactly the default height, so an unwrapped sheet is
/// pixel-identical to one that never heard of this feature.
pub fn wrapped_row_height(lines: u32, default_h: f32) -> f32 {
    let lines = lines.clamp(1, MAX_WRAP_LINES);
    if lines == 1 {
        return default_h;
    }
    // Line spacing, not the full row height per line: the first line already
    // carries the row's vertical padding, so repeating it per line would make
    // a two-line cell twice as tall as it needs to be.
    default_h + (lines - 1) as f32 * (default_h - 6.0).max(8.0)
}

/// Everything about a cell's *presentation* that is not a colour or a font:
/// borders, alignment, indent, wrapping, shrink-to-fit and rotation.
///
/// Deliberately a sibling of [`Typography`] rather than a member of it: type
/// styling answers "what does the ink look like", this answers "where does it
/// go and what is drawn around it". They layer through the same
/// [`CellDecor::apply_to`] discipline and share the same `None`-inherits rule.
///
/// **This is `Copy` and owns no heap.** A `CellDecor` on a column applies to
/// every row of that column including rows that do not exist yet, which is
/// the whole scale argument of this module restated for issue #28: a border
/// over 200M rows is these ~32 bytes, once.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellDecor {
    /// Indexed by [`Side`]. `None` inherits; `Some(Border { style: None, .. })`
    /// explicitly erases whatever a lower scope set.
    pub borders: [Option<Border>; 4],
    /// The diagonal line through the cell, and which way it runs.
    pub diagonal: Option<(Border, Diagonal)>,
    pub h_align: Option<HAlign>,
    pub v_align: Option<VAlign>,
    /// Indent levels, 0..=[`MAX_INDENT`].
    pub indent: Option<u8>,
    pub wrap: Option<bool>,
    pub shrink: Option<bool>,
    /// Degrees, [`MIN_ROTATION`]..=[`MAX_ROTATION`].
    pub rotation: Option<i16>,
}

impl CellDecor {
    pub fn is_empty(&self) -> bool {
        self.borders.iter().all(|b| b.is_none())
            && self.diagonal.is_none()
            && self.h_align.is_none()
            && self.v_align.is_none()
            && self.indent.is_none()
            && self.wrap.is_none()
            && self.shrink.is_none()
            && self.rotation.is_none()
    }

    /// Layer `self` over `out`: set fields win, `None` fields leave `out`
    /// alone. Applied outermost-scope-first — column, then range, then cell
    /// override — so the most specific instruction the user gave is the one
    /// that survives, exactly as [`Typography::apply_to`] does.
    #[inline]
    pub fn apply_to(&self, out: &mut CellDecor) {
        for s in Side::ALL {
            if let Some(b) = self.borders[s.index()] {
                out.borders[s.index()] = Some(b);
            }
        }
        if self.diagonal.is_some() {
            out.diagonal = self.diagonal;
        }
        if self.h_align.is_some() {
            out.h_align = self.h_align;
        }
        if self.v_align.is_some() {
            out.v_align = self.v_align;
        }
        if self.indent.is_some() {
            out.indent = self.indent;
        }
        if self.wrap.is_some() {
            out.wrap = self.wrap;
        }
        if self.shrink.is_some() {
            out.shrink = self.shrink;
        }
        if self.rotation.is_some() {
            out.rotation = self.rotation;
        }
    }

    /// The border on one side, if it draws anything.
    #[inline]
    pub fn border(&self, side: Side) -> Option<Border> {
        self.borders[side.index()].filter(|b| b.is_visible())
    }

    #[inline]
    pub fn wraps(&self) -> bool {
        self.wrap == Some(true)
    }

    #[inline]
    pub fn shrinks(&self) -> bool {
        // Wrap wins: a wrapped cell grows its row instead of shrinking its
        // font, and doing both would fight. Excel resolves it the same way,
        // which is also why `xlsx_loss` reports the combination.
        self.shrink == Some(true) && !self.wraps()
    }

    /// Rotation in degrees, 0 when unset.
    #[inline]
    pub fn rotation_deg(&self) -> i16 {
        self.rotation.unwrap_or(0)
    }

    #[inline]
    pub fn indent_level(&self) -> u8 {
        self.indent.unwrap_or(0)
    }

    /// Left-edge padding this indent adds, in unzoomed pixels.
    #[inline]
    pub fn indent_px(&self) -> f32 {
        self.indent_level() as f32 * INDENT_STEP_PX
    }

    // --- builders, for the toolbar and for tests ---

    pub fn with_border(mut self, side: Side, b: Border) -> Self {
        self.borders[side.index()] = Some(b);
        self
    }

    /// All four sides at once — the "box" button.
    pub fn with_box(mut self, b: Border) -> Self {
        for s in Side::ALL {
            self.borders[s.index()] = Some(b);
        }
        self
    }

    pub fn with_diagonal(mut self, b: Border, d: Diagonal) -> Self {
        self.diagonal = Some((b, d));
        self
    }

    pub fn with_h_align(mut self, a: HAlign) -> Self {
        self.h_align = Some(a);
        self
    }

    pub fn with_v_align(mut self, a: VAlign) -> Self {
        self.v_align = Some(a);
        self
    }

    /// Clamped at the door, so no caller can store an indent the renderer
    /// cannot draw or the exporter cannot write.
    pub fn with_indent(mut self, level: u8) -> Self {
        self.indent = Some(clamp_indent(level));
        self
    }

    pub fn with_wrap(mut self, on: bool) -> Self {
        self.wrap = Some(on);
        self
    }

    pub fn with_shrink(mut self, on: bool) -> Self {
        self.shrink = Some(on);
        self
    }

    /// Clamped to [`MIN_ROTATION`]..=[`MAX_ROTATION`].
    pub fn with_rotation(mut self, deg: i16) -> Self {
        self.rotation = Some(clamp_rotation(deg));
        self
    }
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
    /// Borders, alignment, wrap and rotation for every cell of this column
    /// (issue #28). `Copy` and heap-free, so this is the same handful of
    /// bytes whether the column has 200 rows or 200,000,000.
    pub decor: CellDecor,
}

impl ColumnFormat {
    pub fn is_empty(&self) -> bool {
        self.format == NumberFormat::General && self.rules.is_empty() && self.decor.is_empty()
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
    /// Decoration for every cell in the rectangle (issue #28). The rectangle
    /// is stored, never its cells, so `B2:B50000000` costs one of these.
    pub decor: CellDecor,
}

impl RangeFormat {
    pub fn new(range: TableRange) -> Self {
        Self {
            range,
            format: None,
            rules: Vec::new(),
            decor: CellDecor::default(),
        }
    }

    pub fn with_rule(mut self, rule: ConditionalRule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn with_decor(mut self, decor: CellDecor) -> Self {
        self.decor = decor;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.format.is_none() && self.rules.is_empty() && self.decor.is_empty()
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
    /// Decoration for this one cell — the "border this cell only" case.
    pub decor: CellDecor,
}

impl CellOverride {
    pub fn is_empty(&self) -> bool {
        self.manual.is_empty() && self.format.is_none() && self.decor.is_empty()
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

    /// How many scopes carry a non-empty [`CellDecor`] (issue #28).
    ///
    /// A count of SCOPES, never of cells — the number a scale test can assert
    /// stays at 1 after decorating ten million rows.
    pub fn decor_count(&self) -> usize {
        self.columns
            .values()
            .filter(|c| !c.decor.is_empty())
            .count()
            + self.ranges.iter().filter(|r| !r.decor.is_empty()).count()
            + self
                .overrides
                .values()
                .filter(|o| !o.decor.is_empty())
                .count()
    }

    /// Is any decoration configured anywhere on this sheet?
    ///
    /// The paint loop's short circuit: a sheet with no borders or alignment
    /// never builds a decoration plan at all, so issue #28 costs an
    /// undecorated sheet one boolean per frame.
    pub fn has_decor(&self) -> bool {
        self.decor_count() > 0
    }

    /// Every column that any scope has asked to WRAP, ascending and deduped.
    ///
    /// The row-height calculation needs this and nothing else, and it must be
    /// answerable without knowing which columns are on screen — otherwise a
    /// row's height would change as the user scrolled sideways, and the
    /// height used by the hit test would differ from the one used to paint.
    ///
    /// Bounded by `max_col`, and by `WRAP_COL_SCAN_CAP` overall: a wrap over
    /// a range spanning the whole sheet must not enumerate 16,384 columns on
    /// every frame. Past the cap the extra columns simply do not contribute
    /// to row height, which degrades to "not as tall as it could be" rather
    /// than to a stall.
    pub fn wrapping_cols(&self, max_col: u32, out: &mut Vec<u32>) {
        out.clear();
        for (&c, cf) in &self.columns {
            if cf.decor.wraps() && c <= max_col {
                out.push(c);
            }
        }
        for rf in &self.ranges {
            if !rf.decor.wraps() {
                continue;
            }
            let last = rf.range.last_col.min(max_col);
            for c in rf.range.first_col..=last {
                if out.len() >= WRAP_COL_SCAN_CAP {
                    break;
                }
                out.push(c);
            }
        }
        for ((_, c), ov) in &self.overrides {
            if ov.decor.wraps() && *c <= max_col {
                out.push(*c);
            }
        }
        out.sort_unstable();
        out.dedup();
        out.truncate(WRAP_COL_SCAN_CAP);
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

    // --- decoration: borders, alignment, wrap, rotation (issue #28) ---

    /// Decorate an entire column — borders, alignment, wrap, rotation.
    ///
    /// ONE entry, whatever the row count. This is the operation the scale
    /// criterion is written against: applying it over 10,000,000 rows must
    /// leave [`SheetFormat::heap_bytes`] under a kilobyte, because the store
    /// never learns how many rows the column has.
    ///
    /// LAYERS onto whatever the column already had rather than replacing it,
    /// so "add a bottom border" does not silently clear an alignment set a
    /// moment earlier. Pass a `CellDecor` with an explicit
    /// [`BorderStyle::None`] to erase one edge.
    pub fn set_column_decor(&mut self, col: u32, decor: CellDecor) {
        let entry = self.column_mut(col);
        decor.apply_to(&mut entry.decor);
        self.prune();
    }

    /// Wipe a column's decoration entirely, back to "inherit everything".
    pub fn clear_column_decor(&mut self, col: u32) {
        if let Some(c) = self.columns.get_mut(&col) {
            c.decor = CellDecor::default();
        }
        self.prune();
    }

    pub fn column_decor(&self, col: u32) -> CellDecor {
        self.columns
            .get(&col)
            .map_or(CellDecor::default(), |c| c.decor)
    }

    /// Decorate a rectangle — the "border this selection" command.
    ///
    /// A 200M-cell selection and a 4-cell one produce the same one entry.
    /// Reuses the existing entry for an exact range match, matching
    /// [`SheetFormat::push_rule_for_range`], so decorating the same selection
    /// twice does not grow the store.
    pub fn set_range_decor(&mut self, range: TableRange, decor: CellDecor) -> usize {
        let ri = match self.range_index_of(range) {
            Some(i) => i,
            None => self.push_range(RangeFormat::new(range)),
        };
        let mut merged = self.ranges[ri].decor;
        decor.apply_to(&mut merged);
        self.ranges[ri].decor = merged;
        self.prune();
        // `prune` can drop entries before `ri`, so the index is re-derived
        // rather than assumed — returning a stale index would have the caller
        // edit somebody else's range.
        self.range_index_of(range).unwrap_or(ri)
    }

    /// Decorate one specific cell. The only per-cell decoration path, and
    /// opt-in exactly like [`SheetFormat::set_cell_override`].
    pub fn set_cell_decor(&mut self, cell: CellRef, decor: CellDecor) {
        let e = self.overrides.entry((cell.row, cell.col)).or_default();
        decor.apply_to(&mut e.decor);
        if e.is_empty() {
            self.overrides.remove(&(cell.row, cell.col));
        }
    }

    /// Collect the ordered decorations affecting `col` into `out`.
    ///
    /// Called once per visible column per frame, not per cell — the twin of
    /// [`SheetFormat::plan`], and `out` is caller-owned and reused so a
    /// steady-state repaint allocates nothing here either.
    pub fn decor_plan(&self, col: u32, out: &mut Vec<DecorEntry>) {
        out.clear();
        if let Some(c) = self.columns.get(&col) {
            if !c.decor.is_empty() {
                out.push(DecorEntry {
                    decor: c.decor,
                    rows: None,
                });
            }
        }
        for rf in &self.ranges {
            if col < rf.range.first_col || col > rf.range.last_col || rf.decor.is_empty() {
                continue;
            }
            out.push(DecorEntry {
                decor: rf.decor,
                rows: Some((rf.range.first_row, rf.range.last_row)),
            });
        }
    }

    /// Resolve one cell's decoration from a prepared plan.
    ///
    /// **Allocates nothing.** Column scope first, then ranges in order, then
    /// the per-cell override — the same precedence [`SheetFormat::resolve`]
    /// uses for colour, so a user who has learned one has learned both.
    pub fn resolve_decor(&self, cell: CellRef, plan: &[DecorEntry]) -> CellDecor {
        let mut out = CellDecor::default();
        for e in plan {
            if e.covers(cell.row) {
                e.decor.apply_to(&mut out);
            }
        }
        if let Some(o) = self.overrides.get(&(cell.row, cell.col)) {
            o.decor.apply_to(&mut out);
        }
        out
    }

    /// Resolve a cell's decoration without a prepared plan.
    ///
    /// For callers outside the paint loop (the exporter, the sidecar, tests)
    /// that touch a handful of cells rather than a viewport of them. The
    /// paint loop must use [`SheetFormat::decor_plan`] +
    /// [`SheetFormat::resolve_decor`]: this one walks every range on the
    /// sheet per call.
    pub fn decor_at(&self, cell: CellRef) -> CellDecor {
        let mut plan = Vec::new();
        self.decor_plan(cell.col, &mut plan);
        self.resolve_decor(cell, &plan)
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

    /// Relocate every formatting scope for a row/column INSERT or DELETE.
    ///
    /// All three scopes have to move together, and each moves differently:
    ///
    /// * **column formats** are keyed by a single column index, so they follow
    ///   `map`; a deleted column's format is dropped with it.
    /// * **ranges** are rectangles and follow `map_span`, so inserting inside a
    ///   formatted block extends the format over the new blank cells — which is
    ///   what a user means by inserting a row into a formatted table.
    /// * **cell overrides** are per-cell and follow `map` on the shifted axis.
    ///
    /// Missing any one of these leaves formatting keyed to the pre-change
    /// coordinate: the colours stay on screen column B while the numbers they
    /// were describing moved to C. Cost is O(scopes), never O(rows) — a rule
    /// over a 200M-row column is still one entry here.
    pub fn shift_axis(&mut self, shift: crate::order::AxisShift, axis_is_row: bool) {
        // Column formats only care about a COLUMN shift.
        if !axis_is_row && !self.columns.is_empty() {
            let old = std::mem::take(&mut self.columns);
            for (col, fmt) in old {
                if let Some(dest) = shift.map(col) {
                    self.columns.insert(dest, fmt);
                }
            }
        }
        if !self.ranges.is_empty() {
            let old = std::mem::take(&mut self.ranges);
            self.ranges = old
                .into_iter()
                .filter_map(|mut rf| {
                    let r = rf.range;
                    let moved = if axis_is_row {
                        shift
                            .map_span(r.first_row, r.last_row)
                            .map(|(a, b)| TableRange::new(a, r.first_col, b, r.last_col))
                    } else {
                        shift
                            .map_span(r.first_col, r.last_col)
                            .map(|(a, b)| TableRange::new(r.first_row, a, r.last_row, b))
                    };
                    rf.range = moved?;
                    Some(rf)
                })
                .collect();
        }
        if !self.overrides.is_empty() {
            let old = std::mem::take(&mut self.overrides);
            for ((row, col), ov) in old {
                let dest = if axis_is_row {
                    shift.map(row).map(|r| (r, col))
                } else {
                    shift.map(col).map(|c| (row, c))
                };
                if let Some(key) = dest {
                    self.overrides.insert(key, ov);
                }
            }
        }
        self.prune();
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
