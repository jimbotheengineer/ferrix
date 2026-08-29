//! Structured tables: named ranges with typed columns, validation,
//! formatting, and per-column filtering.
//!
//! A [`Table`] is a rectangular region of a sheet that has been given a name
//! and a schema. It is the Ferrix analogue of an Excel *Table* (the thing
//! `Insert > Table` makes, stored in the `xl/tables/tableN.xml` part), and the
//! whole design here is driven by needing to round-trip through that part
//! without losing anything the user configured.
//!
//! ## Four things a table adds
//!
//! 1. **Identity** — a name, a header row, and per-column names/types, so a
//!    range stops being anonymous cells and starts being a schema.
//! 2. **Validation** — per-column rules ([`ValidationRule`]). Violations are
//!    *reported*, never enforced. See below.
//! 3. **Formatting** — a [`NumberFormat`] per column plus any number of
//!    [`ConditionalRule`]s, resolved per cell into a [`CellStyle`].
//! 4. **Filtering** — per-column [`Predicate`]s compiled into a
//!    [`CompiledPredicate`] and evaluated with the arena-first scan that
//!    `search.rs` uses. See [`RowMask`].
//!
//! ## Validation flags, it does not block
//!
//! Every rule in this module answers "is this cell bad?" and nothing more.
//! There is no path by which a `ValidationRule` refuses a write or rewrites a
//! value. That is deliberate: the common real-world case is *importing* data
//! that already violates the schema, and a spreadsheet that silently drops or
//! coerces the offending cells has destroyed the user's ability to find and
//! fix them. So bad cells stay exactly as typed and get flagged.
//!
//! Two flagging paths exist because they have different cost profiles:
//!
//! * [`Table::validate_cell`] is O(1)-ish and is what the renderer calls for
//!   the ~1,500 cells actually on screen. Painting a red corner on a visible
//!   cell must never depend on having scanned 200M rows.
//! * [`Table::validate`] does a bounded full pass for the "N invalid cells"
//!   badge and the jump-to-next-bad-cell command. It caps its result list and
//!   reports `truncated`, exactly like search does.
//!
//! [`ValidationRule::Unique`] is the exception that needs a whole-column pass
//! even for a single cell, so it is precomputed once into a
//! [`UniquenessIndex`] and consulted in O(1) afterwards. For text columns that
//! index is built over *arena ids*, not strings — duplicate detection on
//! 200M interned cells is a counter array sized by the arena's cardinality.
//!
//! ## Dates
//!
//! Ferrix has no date type; like xlsx, a date is an f64 serial number plus a
//! display format. [`ColumnType::Date`] therefore validates "is a number in
//! the range xlsx can express as a date", and [`NumberFormat::Date`] is what
//! makes it render as one.

use std::collections::HashMap;

use crate::arena::{StrId, StringArena};
use crate::bitmap::Bitmap;
use crate::search::{IdSet, Query};
use crate::sheet::CellRef;
use crate::value::{ErrorKind, Value};

// =========================================================== range & types ==

/// An inclusive rectangular region, in absolute sheet coordinates.
///
/// `first_row` is the header row when [`Table::header_row`] is set, so the
/// data rows are `first_row + 1 ..= last_row`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableRange {
    pub first_row: u32,
    pub last_row: u32,
    pub first_col: u32,
    pub last_col: u32,
}

impl TableRange {
    /// Build a range, normalising reversed corners.
    pub fn new(first_row: u32, first_col: u32, last_row: u32, last_col: u32) -> Self {
        Self {
            first_row: first_row.min(last_row),
            last_row: first_row.max(last_row),
            first_col: first_col.min(last_col),
            last_col: first_col.max(last_col),
        }
    }

    #[inline]
    pub fn rows(&self) -> usize {
        (self.last_row - self.first_row) as usize + 1
    }

    #[inline]
    pub fn cols(&self) -> usize {
        (self.last_col - self.first_col) as usize + 1
    }

    #[inline]
    pub fn contains(&self, cell: CellRef) -> bool {
        cell.row >= self.first_row
            && cell.row <= self.last_row
            && cell.col >= self.first_col
            && cell.col <= self.last_col
    }

    /// A1-style range reference, e.g. `A1:D100` — the spelling the xlsx
    /// `<table ref="...">` attribute wants.
    pub fn to_a1(&self) -> String {
        format!(
            "{}:{}",
            CellRef::new(self.first_row, self.first_col).to_a1(),
            CellRef::new(self.last_row, self.last_col).to_a1()
        )
    }

    /// Parse `A1:D100`. Returns `None` on anything malformed.
    pub fn from_a1(s: &str) -> Option<Self> {
        let (a, b) = s.split_once(':')?;
        let a = CellRef::from_a1(a.trim())?;
        let b = CellRef::from_a1(b.trim())?;
        Some(Self::new(a.row, a.col, b.row, b.col))
    }
}

/// The declared type of a table column.
///
/// This is a *validation* type, not a storage type — the underlying [`Column`]
/// still holds whatever the user typed. Declaring a column `Number` and then
/// pasting text into it produces a flagged cell, not a rejected paste.
///
/// [`Column`]: crate::Column
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ColumnType {
    /// No type constraint.
    #[default]
    Any,
    Number,
    /// A number in the serial-date range. Ferrix has no date type.
    Date,
    Text,
    Bool,
}

impl ColumnType {
    pub const fn as_str(self) -> &'static str {
        match self {
            ColumnType::Any => "any",
            ColumnType::Number => "number",
            ColumnType::Date => "date",
            ColumnType::Text => "text",
            ColumnType::Bool => "bool",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "any" => ColumnType::Any,
            "number" => ColumnType::Number,
            "date" => ColumnType::Date,
            "text" => ColumnType::Text,
            "bool" | "boolean" => ColumnType::Bool,
            _ => return None,
        })
    }

    /// Does `value` satisfy the type? Empty cells always pass here; whether an
    /// empty cell is acceptable is [`Validation::allow_empty`]'s business.
    pub fn accepts(self, value: &Value) -> bool {
        match (self, value) {
            (_, Value::Empty) => true,
            (ColumnType::Any, _) => true,
            (ColumnType::Number, Value::Number(n)) => n.is_finite(),
            (ColumnType::Date, Value::Number(n)) => is_date_serial(*n),
            (ColumnType::Text, Value::Text(_)) => true,
            (ColumnType::Bool, Value::Bool(_)) => true,
            _ => false,
        }
    }
}

/// xlsx serial dates run from 1900-01-01 (1.0) to 9999-12-31 (2958465.0).
/// Excel accepts fractional serials as date+time, so this is a range test
/// rather than an integrality test.
pub const DATE_SERIAL_MIN: f64 = 0.0;
pub const DATE_SERIAL_MAX: f64 = 2_958_465.999_999;

#[inline]
pub fn is_date_serial(n: f64) -> bool {
    n.is_finite() && (DATE_SERIAL_MIN..=DATE_SERIAL_MAX).contains(&n)
}

/// A comparison operator, shared by validation bounds, filter predicates, and
/// conditional-format thresholds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    #[inline]
    pub fn test(self, lhs: f64, rhs: f64) -> bool {
        match self {
            CmpOp::Eq => lhs == rhs,
            CmpOp::Ne => lhs != rhs,
            CmpOp::Lt => lhs < rhs,
            CmpOp::Le => lhs <= rhs,
            CmpOp::Gt => lhs > rhs,
            CmpOp::Ge => lhs >= rhs,
        }
    }

    /// A compact operator glyph, for rule labels in the editor.
    pub const fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "≠",
            CmpOp::Lt => "<",
            CmpOp::Le => "≤",
            CmpOp::Gt => ">",
            CmpOp::Ge => "≥",
        }
    }

    /// The spelling xlsx `dataValidation@operator` / `cfRule@operator` uses.
    pub const fn as_xlsx(self) -> &'static str {
        match self {
            CmpOp::Eq => "equal",
            CmpOp::Ne => "notEqual",
            CmpOp::Lt => "lessThan",
            CmpOp::Le => "lessThanOrEqual",
            CmpOp::Gt => "greaterThan",
            CmpOp::Ge => "greaterThanOrEqual",
        }
    }

    pub fn from_xlsx(s: &str) -> Option<Self> {
        Some(match s {
            "equal" => CmpOp::Eq,
            "notEqual" => CmpOp::Ne,
            "lessThan" => CmpOp::Lt,
            "lessThanOrEqual" => CmpOp::Le,
            "greaterThan" => CmpOp::Gt,
            "greaterThanOrEqual" => CmpOp::Ge,
            _ => return None,
        })
    }
}

// ================================================================ validation ==

/// What a column's cells must satisfy, beyond its [`ColumnType`].
#[derive(Clone, PartialEq, Debug, Default)]
pub enum ValidationRule {
    /// Type check only.
    #[default]
    None,
    /// Numeric (or serial-date) bounds, inclusive.
    Between { min: f64, max: f64 },
    /// Outside inclusive bounds.
    NotBetween { min: f64, max: f64 },
    /// A single numeric comparison.
    Compare { op: CmpOp, value: f64 },
    /// Membership in an explicit list. Compared case-insensitively against the
    /// cell's *display* text so `TRUE`/numbers can be listed too.
    OneOf(Vec<String>),
    /// The cell's display text must match this regular expression, anchored to
    /// the whole cell. Excel has no regex validation, so this exports as a
    /// `custom` rule carrying the pattern in the error message — see the
    /// module docs in `ferrix-io`.
    Regex(String),
    /// Text length bounds, inclusive, on the display text.
    TextLength { min: u32, max: u32 },
    /// No two non-empty cells in the column may be equal.
    Unique,
}

/// A column's complete validation configuration.
#[derive(Clone, PartialEq, Debug)]
pub struct Validation {
    pub rule: ValidationRule,
    /// Empty cells pass regardless of `rule`. Excel's `allowBlank`.
    pub allow_empty: bool,
    /// Shown next to the flag in the UI and exported as the xlsx error text.
    pub message: Option<String>,
}

impl Default for Validation {
    fn default() -> Self {
        Self {
            rule: ValidationRule::None,
            allow_empty: true,
            message: None,
        }
    }
}

impl Validation {
    pub fn new(rule: ValidationRule) -> Self {
        Self {
            rule,
            ..Default::default()
        }
    }

    pub fn allow_empty(mut self, yes: bool) -> Self {
        self.allow_empty = yes;
        self
    }

    pub fn message(mut self, m: impl Into<String>) -> Self {
        self.message = Some(m.into());
        self
    }

    /// True when this configuration can never reject anything, so callers can
    /// skip the whole machinery.
    pub fn is_vacuous(&self) -> bool {
        matches!(self.rule, ValidationRule::None) && self.allow_empty
    }
}

/// Why a cell was flagged. Carries enough to write a human sentence without
/// re-deriving the rule.
#[derive(Clone, PartialEq, Debug)]
pub enum Violation {
    /// The cell is empty and the column requires a value.
    Empty,
    /// Wrong type for the column.
    WrongType(ColumnType),
    /// Outside the allowed numeric range.
    OutOfRange { min: f64, max: f64 },
    /// Failed a single comparison.
    FailsCompare { op: CmpOp, value: f64 },
    /// Not one of the allowed values.
    NotInList,
    /// Did not match the pattern.
    RegexMismatch,
    /// Text length outside bounds.
    BadLength { min: u32, max: u32, got: u32 },
    /// Duplicate of another cell in the column.
    Duplicate,
    /// The cell holds a spreadsheet error value.
    ErrorValue(ErrorKind),
}

impl Violation {
    /// A short sentence for a tooltip or the status bar.
    pub fn describe(&self) -> String {
        match self {
            Violation::Empty => "value required".into(),
            Violation::WrongType(t) => format!("expected {}", t.as_str()),
            Violation::OutOfRange { min, max } => {
                format!("must be between {} and {}", fmt_f64(*min), fmt_f64(*max))
            }
            Violation::FailsCompare { op, value } => {
                format!("must be {} {}", op.as_xlsx(), fmt_f64(*value))
            }
            Violation::NotInList => "not an allowed value".into(),
            Violation::RegexMismatch => "does not match the required pattern".into(),
            Violation::BadLength { min, max, got } => {
                format!("length {got} outside {min}..={max}")
            }
            Violation::Duplicate => "duplicate value".into(),
            Violation::ErrorValue(e) => format!("cell holds {}", e.as_str()),
        }
    }
}

fn fmt_f64(v: f64) -> String {
    crate::value::format_number(v)
}

/// Outcome of a full-table validation pass.
///
/// Bounded exactly like [`crate::SearchResults`]: a 200M-row table where every
/// row is bad would otherwise try to build a 200M-entry `Vec`.
#[derive(Clone, Debug, Default)]
pub struct ValidationReport {
    /// Flagged cells in row-major order, capped at the caller's limit.
    pub invalid: Vec<(CellRef, Violation)>,
    /// True count of flagged cells, which may exceed `invalid.len()`.
    pub total: usize,
    pub truncated: bool,
    pub millis: u128,
}

impl ValidationReport {
    pub fn is_clean(&self) -> bool {
        self.total == 0
    }

    /// The first flagged cell at or after `cell`, for "jump to next problem".
    pub fn next_after(&self, cell: CellRef) -> Option<CellRef> {
        self.invalid
            .iter()
            .map(|(c, _)| *c)
            .find(|c| (c.row, c.col) > (cell.row, cell.col))
            .or_else(|| self.invalid.first().map(|(c, _)| *c))
    }
}

/// Precomputed duplicate detection for a [`ValidationRule::Unique`] column.
///
/// Built once per column per validation pass. Text is handled by *arena id*:
/// two text cells are equal iff their ids are equal, because the arena
/// interns. So the index is a counter array sized by the arena's cardinality
/// (18 entries on the 200M-row benchmark), not by the row count.
#[derive(Clone, Debug, Default)]
pub struct UniquenessIndex {
    /// Occurrence count per arena id.
    text_counts: Vec<u32>,
    /// Occurrence count per distinct numeric bit pattern. Numbers are not
    /// interned, so this one really is cardinality-of-the-column sized; that
    /// is unavoidable for numeric uniqueness and is why `Unique` on a numeric
    /// column is documented as the expensive rule.
    num_counts: HashMap<u64, u32>,
    bool_counts: [u32; 2],
    built: bool,
}

impl UniquenessIndex {
    pub fn new(arena_len: usize) -> Self {
        Self {
            text_counts: vec![0; arena_len],
            num_counts: HashMap::new(),
            bool_counts: [0; 2],
            built: true,
        }
    }

    /// Record one cell.
    pub fn observe(&mut self, value: &Value) {
        match value {
            Value::Text(id) => {
                if let Some(slot) = self.text_counts.get_mut(id.0 as usize) {
                    *slot = slot.saturating_add(1);
                }
            }
            Value::Number(n) => {
                // Normalise -0.0 and NaN so bit-pattern keying behaves.
                let key = (if *n == 0.0 { 0.0 } else { *n }).to_bits();
                *self.num_counts.entry(key).or_insert(0) += 1;
            }
            Value::Bool(b) => self.bool_counts[*b as usize] += 1,
            _ => {}
        }
    }

    /// Is this value a duplicate within the observed set?
    pub fn is_duplicate(&self, value: &Value) -> bool {
        if !self.built {
            return false;
        }
        match value {
            Value::Text(id) => self.text_counts.get(id.0 as usize).is_some_and(|&c| c > 1),
            Value::Number(n) => {
                let key = (if *n == 0.0 { 0.0 } else { *n }).to_bits();
                self.num_counts.get(&key).is_some_and(|&c| c > 1)
            }
            Value::Bool(b) => self.bool_counts[*b as usize] > 1,
            _ => false,
        }
    }
}

// ================================================================ formatting ==

/// An RGB colour. Kept dependency-free; the UI converts to `egui::Color32`
/// and the exporter to an xlsx `rgb="FFRRGGBB"` string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn to_u32(self) -> u32 {
        ((self.0 as u32) << 16) | ((self.1 as u32) << 8) | self.2 as u32
    }

    pub const fn from_u32(v: u32) -> Self {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// `RRGGBB`, as xlsx and CSS both spell it.
    pub fn to_hex(self) -> String {
        format!("{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }

    /// Parse `RRGGBB` or `FFRRGGBB` (xlsx writes the alpha byte first).
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        let s = match s.len() {
            6 => s,
            8 => &s[2..],
            _ => return None,
        };
        let v = u32::from_str_radix(s, 16).ok()?;
        Some(Rgb::from_u32(v))
    }

    /// Linear interpolation, for colour scales.
    pub fn lerp(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(
            mix(self.0, other.0),
            mix(self.1, other.1),
            mix(self.2, other.2),
        )
    }
}

/// A number format.
///
/// # The supported subset, and why unknown formats are kept verbatim
///
/// Excel's format grammar is a small language: section separators, colour
/// codes, conditions, locale hints, fraction and scientific forms, per-section
/// text substitutions. Ferrix implements the handful of shapes people actually
/// configure through a UI, and represents everything else as
/// [`NumberFormat::Custom`], which stores the format string **exactly as it
/// arrived** and writes it back out unchanged.
///
/// That asymmetry is on purpose. Rendering a format we do not understand is a
/// cosmetic failure — the cell shows a plain number. *Dropping* it is data
/// loss: the user's `[$€-407]#,##0.00;[RED]-#,##0.00` is gone and they cannot
/// get it back. So the parser is allowed to not understand a format, but the
/// writer is never allowed to forget one.
///
/// Implemented subset:
///
/// | variant     | format code            |
/// |-------------|------------------------|
/// | `General`   | `General`              |
/// | `Decimal`   | `0`, `0.00`, ...       |
/// | `Thousands` | `#,##0`, `#,##0.00`    |
/// | `Currency`  | `"$"#,##0.00`          |
/// | `Percent`   | `0%`, `0.00%`          |
/// | `Date`      | `yyyy\-mm\-dd` etc.    |
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum NumberFormat {
    #[default]
    General,
    /// Fixed decimal places, no grouping.
    Decimal {
        places: u8,
    },
    /// Thousands separators.
    Thousands {
        places: u8,
    },
    /// A leading currency symbol plus thousands separators.
    Currency {
        symbol: String,
        places: u8,
    },
    /// Value multiplied by 100 with a `%` suffix, as Excel does.
    Percent {
        places: u8,
    },
    Date(DateStyle),
    /// Any format string Ferrix does not model. Preserved byte-for-byte.
    Custom(String),
}

/// The date shapes with a first-class representation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DateStyle {
    /// `yyyy-mm-dd`
    Iso,
    /// `mm/dd/yyyy`
    Us,
    /// `dd/mm/yyyy`
    Euro,
    /// `yyyy-mm-dd hh:mm:ss`
    IsoDateTime,
    /// `hh:mm:ss`
    Time,
}

impl DateStyle {
    pub const fn code(self) -> &'static str {
        match self {
            DateStyle::Iso => "yyyy-mm-dd",
            DateStyle::Us => "mm/dd/yyyy",
            DateStyle::Euro => "dd/mm/yyyy",
            DateStyle::IsoDateTime => "yyyy-mm-dd hh:mm:ss",
            DateStyle::Time => "hh:mm:ss",
        }
    }

    pub fn from_code(s: &str) -> Option<Self> {
        const ALL: [DateStyle; 5] = [
            DateStyle::Iso,
            DateStyle::Us,
            DateStyle::Euro,
            DateStyle::IsoDateTime,
            DateStyle::Time,
        ];
        ALL.into_iter().find(|d| d.code() == s)
    }
}

impl NumberFormat {
    /// The xlsx format code for this format.
    pub fn to_code(&self) -> String {
        match self {
            NumberFormat::General => "General".to_string(),
            NumberFormat::Decimal { places } => zeros(*places),
            NumberFormat::Thousands { places } => format!("#,##{}", zeros(*places)),
            NumberFormat::Currency { symbol, places } => {
                format!("\"{}\"#,##{}", symbol, zeros(*places))
            }
            NumberFormat::Percent { places } => format!("{}%", zeros(*places)),
            NumberFormat::Date(d) => d.code().to_string(),
            NumberFormat::Custom(s) => s.clone(),
        }
    }

    /// Parse an xlsx format code into the modelled subset, falling back to
    /// [`NumberFormat::Custom`] with the string intact.
    ///
    /// The fallback is not a failure mode — it is the contract. Everything
    /// this function does not recognise still round-trips.
    pub fn from_code(code: &str) -> Self {
        if code.eq_ignore_ascii_case("general") || code.is_empty() {
            return NumberFormat::General;
        }
        if let Some(d) = DateStyle::from_code(code) {
            return NumberFormat::Date(d);
        }
        // Percent: `0%`, `0.00%`
        if let Some(head) = code.strip_suffix('%') {
            if let Some(places) = places_of(head) {
                return NumberFormat::Percent { places };
            }
        }
        // Currency: `"$"#,##0.00`
        if let Some(rest) = code.strip_prefix('"') {
            if let Some((symbol, tail)) = rest.split_once('"') {
                if let Some(places) = grouped_places(tail) {
                    return NumberFormat::Currency {
                        symbol: symbol.to_string(),
                        places,
                    };
                }
            }
        }
        if let Some(places) = grouped_places(code) {
            return NumberFormat::Thousands { places };
        }
        if let Some(places) = places_of(code) {
            return NumberFormat::Decimal { places };
        }
        NumberFormat::Custom(code.to_string())
    }

    /// Is this a date format? Drives whether the grid renders a serial number
    /// as a calendar date.
    pub fn is_date(&self) -> bool {
        match self {
            NumberFormat::Date(_) => true,
            // A custom code that only mentions date/time fields is a date too;
            // this is how an Excel-authored `d-mmm-yy` still renders sensibly.
            NumberFormat::Custom(s) => {
                let low = s.to_ascii_lowercase();
                low.contains('y') || low.contains('d') || low.contains("hh")
            }
            _ => false,
        }
    }

    /// Render a value. Unknown custom formats fall back to the plain number —
    /// cosmetic degradation, never mutation of the stored value.
    pub fn render(&self, v: f64) -> String {
        match self {
            NumberFormat::General => crate::value::format_number(v),
            NumberFormat::Decimal { places } => format!("{v:.*}", *places as usize),
            NumberFormat::Thousands { places } => group_thousands(v, *places, ""),
            NumberFormat::Currency { symbol, places } => group_thousands(v, *places, symbol),
            NumberFormat::Percent { places } => {
                format!("{:.*}%", *places as usize, v * 100.0)
            }
            NumberFormat::Date(d) => render_serial(v, *d),
            NumberFormat::Custom(_) => crate::value::format_number(v),
        }
    }
}

/// `0`, `0.00`, `0.000`, ...
fn zeros(places: u8) -> String {
    if places == 0 {
        "0".to_string()
    } else {
        format!("0.{}", "0".repeat(places as usize))
    }
}

/// Decimal places of a bare `0`/`0.00`-shaped code, or `None` if not that shape.
fn places_of(code: &str) -> Option<u8> {
    if code == "0" {
        return Some(0);
    }
    let rest = code.strip_prefix("0.")?;
    (!rest.is_empty() && rest.bytes().all(|b| b == b'0')).then_some(rest.len() as u8)
}

/// Decimal places of a `#,##0`/`#,##0.00`-shaped code.
fn grouped_places(code: &str) -> Option<u8> {
    let rest = code.strip_prefix("#,##")?;
    places_of(rest)
}

fn group_thousands(v: f64, places: u8, prefix: &str) -> String {
    let neg = v < 0.0;
    let s = format!("{:.*}", places as usize, v.abs());
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s.as_str(), None),
    };
    let mut grouped = String::with_capacity(int.len() + int.len() / 3 + 4);
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(prefix);
    out.push_str(&grouped);
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    out
}

/// Convert an xlsx serial number to a calendar date and render it.
///
/// Implements the 1900 date system *including* Excel's deliberate
/// 1900-02-29 bug: serial 60 is a day that never existed, and every date after
/// it is therefore offset by one. Reproducing the bug is the only way for
/// serials to agree with Excel, which is the entire point of the format.
pub fn render_serial(serial: f64, style: DateStyle) -> String {
    if !serial.is_finite() {
        return crate::value::format_number(serial);
    }
    let days = serial.floor();
    let frac = serial - days;
    let secs_total = (frac * 86_400.0).round() as i64;
    let (hh, mm, ss) = (secs_total / 3600, (secs_total % 3600) / 60, secs_total % 60);
    if style == DateStyle::Time {
        return format!("{hh:02}:{mm:02}:{ss:02}");
    }
    let (y, m, d) = civil_from_serial(days as i64);
    match style {
        DateStyle::Iso => format!("{y:04}-{m:02}-{d:02}"),
        DateStyle::Us => format!("{m:02}/{d:02}/{y:04}"),
        DateStyle::Euro => format!("{d:02}/{m:02}/{y:04}"),
        DateStyle::IsoDateTime => {
            format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
        }
        DateStyle::Time => unreachable!("handled above"),
    }
}

/// Serial day number -> (year, month, day), 1900 date system with the leap bug.
///
/// Serial 1 is 1900-01-01. Serial 60 is Excel's phantom 1900-02-29, which we
/// report as such so the number the user sees matches Excel's. From serial 61
/// on, the serial is two days ahead of the true day count from 1899-12-31.
fn civil_from_serial(serial: i64) -> (i64, u32, u32) {
    if serial == 60 {
        return (1900, 2, 29);
    }
    // Unix day number for the serial. 1900-01-01 (serial 1) is Unix day
    // -25_567 and 1900-03-01 (serial 61) is Unix day -25_508; the one-day
    // discontinuity between them is the phantom leap day.
    let unix_days = serial - 25_569 + i64::from(serial < 60);
    civil_from_days(unix_days)
}

/// Howard Hinnant's `civil_from_days`, with 1970-01-01 as day 0.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A conditional formatting rule over a column or range.
///
/// Rules are always evaluated as an ordered list and a later rule overrides an
/// earlier one, which is Excel's own precedence and the reason the rules
/// editor makes the order draggable rather than hiding it.
///
/// [`ConditionalRule::Manual`] is in here rather than in a parallel
/// "static colours" mechanism on purpose: a hand-picked colour is just a rule
/// whose condition is *true*. Unifying them means one ordered list, one
/// precedence story, one persistence path, and one export path — and it means
/// a manual colour and a value-driven rule interact in a way the user can
/// predict from where they sit in the list.
#[derive(Clone, PartialEq, Debug)]
pub enum ConditionalRule {
    /// An unconditional colour — the "paint it yellow" case.
    Manual {
        fill: Option<Rgb>,
        text: Option<Rgb>,
        /// Type styling applied with the colours, so one "format this
        /// selection" action covers both in a single rule.
        typography: crate::format::Typography,
    },
    /// Two-colour scale between the column's min and max.
    ColorScale2 { min: Rgb, max: Rgb },
    /// Three-colour scale; the midpoint is the 50th percentile of the range.
    ColorScale3 { min: Rgb, mid: Rgb, max: Rgb },
    /// A proportional bar drawn behind the value.
    DataBar { color: Rgb },
    /// Fill/text colours applied when a comparison holds.
    Threshold {
        op: CmpOp,
        value: f64,
        fill: Rgb,
        text: Rgb,
    },
    /// Colour by sign — the negative-red / positive-green convention.
    ///
    /// Expressible as two [`ConditionalRule::Threshold`]s, but kept distinct
    /// because it is the single most common thing a user wants, and because
    /// one rule with three optional colours is far easier to present in an
    /// editor than two rules that must be kept consistent.
    Sign {
        negative: Option<Rgb>,
        positive: Option<Rgb>,
        zero: Option<Rgb>,
    },
    /// The N largest (or smallest) values in the evaluated window.
    ///
    /// Rank cannot be answered from one cell, so this depends on
    /// [`crate::format::RuleEval::cut`] and no-ops without it.
    TopBottom {
        top: bool,
        n: u32,
        fill: Rgb,
        text: Rgb,
    },
    /// The cell's display text contains `needle`, compared case-insensitively.
    TextContains {
        needle: String,
        fill: Rgb,
        text: Rgb,
    },
}

/// The visual result of applying every conditional rule to one cell.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct CellStyle {
    pub fill: Option<Rgb>,
    pub text: Option<Rgb>,
    /// Bar fraction in `0.0..=1.0`, and its colour.
    pub bar: Option<(f32, Rgb)>,
    /// Resolved type styling. Empty means "use the grid default", which is
    /// the overwhelmingly common case and costs nothing to represent.
    pub typography: crate::format::Typography,
}

impl CellStyle {
    pub fn is_plain(&self) -> bool {
        self.fill.is_none()
            && self.text.is_none()
            && self.bar.is_none()
            && self.typography.is_empty()
    }
}

impl ConditionalRule {
    /// Does this rule need the column's `(min, max)` over the evaluated
    /// window? Colour scales and bars are meaningless without it.
    #[inline]
    pub fn needs_extent(&self) -> bool {
        matches!(
            self,
            ConditionalRule::ColorScale2 { .. }
                | ConditionalRule::ColorScale3 { .. }
                | ConditionalRule::DataBar { .. }
        )
    }

    /// Does this rule need the cell's *display text*?
    ///
    /// Resolving display text allocates, so callers ask this before paying for
    /// it and pass `""` when the answer is no.
    #[inline]
    pub fn needs_text(&self) -> bool {
        matches!(self, ConditionalRule::TextContains { .. })
    }

    /// Does this rule need a scan of the visible window before it can be
    /// evaluated at all? `false` means the rule is answerable from one cell,
    /// which is the case for every rule except scales, bars and top/bottom-N.
    #[inline]
    pub fn needs_window(&self) -> bool {
        self.needs_extent() || matches!(self, ConditionalRule::TopBottom { .. })
    }

    /// A short human label for the rules editor.
    pub fn label(&self) -> String {
        match self {
            ConditionalRule::Manual { fill, text, .. } => match (fill, text) {
                (Some(_), Some(_)) => "Fill + text colour".into(),
                (Some(_), None) => "Fill colour".into(),
                (None, Some(_)) => "Text colour".into(),
                (None, None) => "Manual (no colour)".into(),
            },
            ConditionalRule::ColorScale2 { .. } => "2-colour scale".into(),
            ConditionalRule::ColorScale3 { .. } => "3-colour scale".into(),
            ConditionalRule::DataBar { .. } => "Data bar".into(),
            ConditionalRule::Threshold { op, value, .. } => {
                format!(
                    "Value {} {}",
                    op.symbol(),
                    crate::value::format_number(*value)
                )
            }
            ConditionalRule::Sign { .. } => "Colour by sign".into(),
            ConditionalRule::TopBottom { top, n, .. } => {
                format!("{} {}", if *top { "Top" } else { "Bottom" }, n)
            }
            ConditionalRule::TextContains { needle, .. } => {
                format!("Text contains \"{needle}\"")
            }
        }
    }

    /// Apply this rule to a numeric cell.
    ///
    /// `extent` is the column's `(min, max)` over the visible rows. Scales and
    /// bars are meaningless without it, so they no-op when it is `None`.
    ///
    /// This is the table-facing entry point and is kept for the rules a table
    /// column can express; sheet-level formatting goes through
    /// [`ConditionalRule::apply_cell`], which also handles text and rank.
    pub fn apply(&self, v: f64, extent: Option<(f64, f64)>, out: &mut CellStyle) {
        self.apply_cell(
            &Value::Number(v),
            "",
            crate::format::RuleEval { extent, cut: None },
            out,
        );
    }

    /// Apply this rule to any cell value.
    ///
    /// Allocates nothing. `text` is the cell's display text when
    /// [`ConditionalRule::needs_text`] said it was wanted, and `""` otherwise —
    /// a text rule handed an empty string simply does not match, which is the
    /// right answer for an empty cell too.
    pub fn apply_cell(
        &self,
        value: &Value,
        text: &str,
        eval: crate::format::RuleEval,
        out: &mut CellStyle,
    ) {
        // A manual colour is unconditional and so is the only rule that
        // applies to a non-numeric cell without further thought.
        if let ConditionalRule::Manual {
            fill,
            text: tc,
            typography,
        } = self
        {
            if let Some(f) = fill {
                out.fill = Some(*f);
            }
            if let Some(t) = tc {
                out.text = Some(*t);
            }
            typography.apply_to(&mut out.typography);
            return;
        }

        if let ConditionalRule::TextContains {
            needle,
            fill,
            text: tc,
        } = self
        {
            // Case-insensitive without allocating: `to_lowercase` on every
            // painted cell would be a per-frame string churn for nothing.
            if !needle.is_empty() && contains_ignore_ascii_case(text, needle) {
                out.fill = Some(*fill);
                out.text = Some(*tc);
            }
            return;
        }

        let Value::Number(v) = value else { return };
        let v = *v;

        let frac = || -> Option<f32> {
            let (lo, hi) = eval.extent?;
            // A degenerate or NaN extent has no meaningful position in it;
            // everything sits at the bottom rather than dividing by zero.
            if !(hi - lo).is_finite() || hi <= lo {
                return Some(0.0);
            }
            Some((((v - lo) / (hi - lo)) as f32).clamp(0.0, 1.0))
        };
        match self {
            ConditionalRule::Manual { .. } | ConditionalRule::TextContains { .. } => {
                unreachable!("handled above")
            }
            ConditionalRule::ColorScale2 { min, max } => {
                if let Some(t) = frac() {
                    out.fill = Some(min.lerp(*max, t));
                }
            }
            ConditionalRule::ColorScale3 { min, mid, max } => {
                if let Some(t) = frac() {
                    out.fill = Some(if t < 0.5 {
                        min.lerp(*mid, t * 2.0)
                    } else {
                        mid.lerp(*max, (t - 0.5) * 2.0)
                    });
                }
            }
            ConditionalRule::DataBar { color } => {
                if let Some(t) = frac() {
                    out.bar = Some((t, *color));
                }
            }
            ConditionalRule::Threshold {
                op,
                value,
                fill,
                text,
            } => {
                if op.test(v, *value) {
                    out.fill = Some(*fill);
                    out.text = Some(*text);
                }
            }
            ConditionalRule::Sign {
                negative,
                positive,
                zero,
            } => {
                // Only the TEXT colour is set. Sign colouring is a typographic
                // convention (accountants' red), not a highlight, and filling
                // every negative cell would drown the sheet in colour.
                let pick = if v < 0.0 {
                    *negative
                } else if v > 0.0 {
                    *positive
                } else {
                    *zero
                };
                if let Some(c) = pick {
                    out.text = Some(c);
                }
            }
            ConditionalRule::TopBottom {
                top,
                fill,
                text,
                n: _,
            } => {
                // No rank cut means the caller declined to scan a window; the
                // rule then says nothing rather than guessing.
                if let Some(cut) = eval.cut {
                    let hit = if *top { v >= cut } else { v <= cut };
                    if hit {
                        out.fill = Some(*fill);
                        out.text = Some(*text);
                    }
                }
            }
        }
    }
}

/// `haystack.to_lowercase().contains(&needle.to_lowercase())` without the two
/// allocations, which matters at ~1,500 calls per frame.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return n.is_empty();
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

// ================================================================= filtering ==

/// A per-column filter predicate, before compilation.
#[derive(Clone, PartialEq, Debug)]
pub enum Predicate {
    /// A checklist of allowed display values, as Excel's filter dropdown
    /// offers. Compared case-insensitively.
    ValueList(Vec<String>),
    /// Numeric (or serial-date) comparison.
    Compare { op: CmpOp, value: f64 },
    /// Inclusive numeric range.
    Between { min: f64, max: f64 },
    /// Substring or whole-cell text match. Compiles to the same [`Query`] the
    /// search box uses, so it gets the arena fast path for free.
    Text {
        needle: String,
        case_sensitive: bool,
        whole_cell: bool,
    },
    /// Only empty cells.
    Blank,
    /// Only non-empty cells.
    NonBlank,
}

/// A predicate compiled against a specific arena.
///
/// This is where the arena-first trick is reused. A [`Predicate::ValueList`]
/// or [`Predicate::Text`] over a 200M-row column never compares a string
/// per row: the predicate is matched once against every *distinct* string in
/// the arena, producing an [`IdSet`], and the column scan then compares 4-byte
/// ids against that bitset. Cost tracks cardinality, not row count — exactly
/// as documented in `search.rs`.
///
/// The flags let a scanner skip an entire column: a numeric-only predicate
/// over a text-only column, or a text predicate whose `IdSet` came back empty,
/// cannot produce a single hit.
#[derive(Clone, Debug)]
pub struct CompiledPredicate {
    ids: IdSet,
    /// Numeric test, if the predicate can match numbers at all.
    num: Option<NumTest>,
    /// Which booleans pass.
    bools: [bool; 2],
    /// Whether empty cells pass.
    empty: bool,
    /// Whether error cells can pass (only a text predicate matching the
    /// error's spelling, or a value checklist containing it).
    errors: [bool; 8],
    /// True when no arena string matched, so text cells can be skipped wholesale.
    text_dead: bool,
    /// Numeric entries of a value checklist. Small by construction (it is a
    /// UI checklist), so a linear probe beats a hash lookup and allocates
    /// nothing per cell.
    list: Vec<f64>,
}

#[derive(Clone, Copy, Debug)]
enum NumTest {
    Cmp {
        op: CmpOp,
        value: f64,
    },
    Between {
        min: f64,
        max: f64,
    },
    /// Any number passes (a NonBlank filter).
    Any,
    /// A value checklist that contained numeric-looking entries. The list is
    /// small by construction (it is a UI checklist), so a linear probe over it
    /// is cheaper than a hash lookup and has no allocation.
    OneOf,
}

impl CompiledPredicate {
    /// Compile `pred` against `arena`.
    pub fn compile(pred: &Predicate, arena: &StringArena) -> Self {
        Self::compile_with(pred, arena.len(), |id| {
            arena.resolve_or_empty(StrId(id)).to_string()
        })
    }

    /// Compile against an arbitrary arena representation, so the memory-mapped
    /// reader (whose arena lives inside the mapping) can use the same path.
    pub fn compile_with<F: Fn(u32) -> String>(
        pred: &Predicate,
        arena_len: usize,
        resolve: F,
    ) -> Self {
        let mut me = Self {
            ids: IdSet::default(),
            num: None,
            bools: [false; 2],
            empty: false,
            errors: [false; 8],
            text_dead: true,
            list: Vec::new(),
        };
        match pred {
            Predicate::Blank => {
                me.empty = true;
            }
            Predicate::NonBlank => {
                me.num = Some(NumTest::Any);
                me.bools = [true; 2];
                me.errors = [true; 8];
                me.ids = IdSet::from_pairs(
                    arena_len,
                    (0..arena_len as u32).map(|i| (i, "x")),
                    // Every string is non-blank, so match them all. Using a
                    // trivially-true query keeps this on the same code path.
                    &Query::new("x", false, false).expect("non-empty needle"),
                );
                me.text_dead = arena_len == 0;
            }
            Predicate::Compare { op, value } => {
                me.num = Some(NumTest::Cmp {
                    op: *op,
                    value: *value,
                });
            }
            Predicate::Between { min, max } => {
                me.num = Some(NumTest::Between {
                    min: *min,
                    max: *max,
                });
            }
            Predicate::Text {
                needle,
                case_sensitive,
                whole_cell,
            } => {
                if let Some(q) = Query::new(needle, *case_sensitive, *whole_cell) {
                    let owned: Vec<String> = (0..arena_len as u32).map(&resolve).collect();
                    me.ids = IdSet::from_pairs(
                        arena_len,
                        owned
                            .iter()
                            .enumerate()
                            .map(|(i, s)| (i as u32, s.as_str())),
                        &q,
                    );
                    me.text_dead = me.ids.is_empty();
                    me.bools = [q.matches_bool(false), q.matches_bool(true)];
                    for (i, e) in ALL_ERRORS.iter().enumerate() {
                        me.errors[i] = q.matches_str(e.as_str());
                    }
                }
            }
            Predicate::ValueList(values) => {
                let wanted: Vec<String> = values.iter().map(|v| v.to_lowercase()).collect();
                let owned: Vec<String> = (0..arena_len as u32).map(&resolve).collect();
                me.ids = IdSet::from_pairs_pred(arena_len, owned.iter().enumerate(), |s| {
                    wanted.iter().any(|w| w == &s.to_lowercase())
                });
                me.text_dead = me.ids.is_empty();
                me.bools = [
                    wanted.iter().any(|w| w == "false"),
                    wanted.iter().any(|w| w == "true"),
                ];
                for (i, e) in ALL_ERRORS.iter().enumerate() {
                    me.errors[i] = wanted.iter().any(|w| w == &e.as_str().to_lowercase());
                }
                me.list = values
                    .iter()
                    .filter_map(|v| v.trim().parse::<f64>().ok())
                    .collect();
                if !me.list.is_empty() {
                    me.num = Some(NumTest::OneOf);
                }
                me.empty = wanted.iter().any(|w| w.is_empty());
            }
        }
        me
    }

    /// Can any text cell match? Lets a scanner skip a whole text column.
    #[inline]
    pub fn can_match_text(&self) -> bool {
        !self.text_dead
    }

    /// Can any numeric cell match?
    #[inline]
    pub fn can_match_numbers(&self) -> bool {
        self.num.is_some()
    }

    #[inline]
    pub fn matches_text_id(&self, id: u32) -> bool {
        !self.text_dead && self.ids.contains(id)
    }

    #[inline]
    pub fn matches_number(&self, v: f64) -> bool {
        match self.num {
            None => false,
            Some(NumTest::Any) => true,
            Some(NumTest::Cmp { op, value }) => op.test(v, value),
            Some(NumTest::Between { min, max }) => v >= min && v <= max,
            Some(NumTest::OneOf) => self.list.contains(&v),
        }
    }

    #[inline]
    pub fn matches_bool(&self, b: bool) -> bool {
        self.bools[b as usize]
    }

    #[inline]
    pub fn matches_empty(&self) -> bool {
        self.empty
    }

    #[inline]
    pub fn matches_error(&self, e: ErrorKind) -> bool {
        self.errors[e.to_code() as usize & 7]
    }

    /// Evaluate against a fully-resolved value. The small-file and test path;
    /// the hot path uses the tag-dispatched accessors above.
    pub fn matches_value(&self, v: &Value) -> bool {
        match v {
            Value::Empty => self.matches_empty(),
            Value::Number(n) => self.matches_number(*n),
            Value::Bool(b) => self.matches_bool(*b),
            Value::Text(id) => self.matches_text_id(id.0),
            Value::Error(e) => self.matches_error(*e),
        }
    }

    /// Distinct arena strings this predicate matched — the number that
    /// explains why filtering 200M rows was instant.
    pub fn matched_strings(&self) -> usize {
        self.ids.len()
    }
}

const ALL_ERRORS: [ErrorKind; 8] = [
    ErrorKind::DivZero,
    ErrorKind::Value,
    ErrorKind::Ref,
    ErrorKind::Name,
    ErrorKind::Num,
    ErrorKind::NotAvailable,
    ErrorKind::Null,
    ErrorKind::Circular,
];

/// Which rows survive a filter.
///
/// # Why a bitmap and not a `Vec<u32>`
///
/// A filter on a 200M-row table that keeps half the rows would need a 400MB
/// `Vec<u32>` of row indices. A bitmap costs one bit per row — 25MB for the
/// whole 200M — regardless of how many rows match, so the memory cost is a
/// function of the table's height alone and cannot blow up on a permissive
/// filter. Nothing is copied; the base data is untouched.
///
/// # Mapping a view row to a data row
///
/// The renderer needs "what is the 5,000,000th visible row?" every frame. A
/// bitmap alone answers that in O(rows). So the mask also carries a rank
/// index: the number of set bits before each 4,096-row block. At 200M rows
/// that is 48,829 `u64`s (≈390 KB), and [`RowMask::nth_visible`] becomes a
/// binary search over the index plus a scan of at most one block — bounded
/// work per lookup no matter the table height.
#[derive(Clone, Debug, Default)]
pub struct RowMask {
    bits: Bitmap,
    /// Set bits strictly before block `i`. `block_prefix[0] == 0`.
    block_prefix: Vec<u64>,
    visible: usize,
    /// Rows the filter actually examined; less than `bits.len()` only when the
    /// caller bounded the scan.
    scanned: usize,
    /// True when the scan was cut short by a row budget, so `visible` is a
    /// lower bound rather than the full count.
    truncated: bool,
    millis: u128,
}

/// Rows per rank-index block. 4,096 keeps the index under 0.5 MB at 200M rows
/// while bounding the in-block scan to 64 words.
pub const RANK_BLOCK: usize = 4096;

impl RowMask {
    /// A mask where every row in `0..rows` is visible — the unfiltered state.
    pub fn all_visible(rows: usize) -> Self {
        let mut me = Self {
            bits: Bitmap::ones(rows),
            block_prefix: Vec::new(),
            visible: rows,
            scanned: rows,
            truncated: false,
            millis: 0,
        };
        me.build_index();
        me
    }

    /// Build from a bitmap of accepted rows.
    pub fn from_bits(bits: Bitmap) -> Self {
        let visible = bits.count_set();
        let scanned = bits.len();
        let mut me = Self {
            bits,
            block_prefix: Vec::new(),
            visible,
            scanned,
            truncated: false,
            millis: 0,
        };
        me.build_index();
        me
    }

    pub fn with_stats(mut self, scanned: usize, truncated: bool, millis: u128) -> Self {
        self.scanned = scanned;
        self.truncated = truncated;
        self.millis = millis;
        self
    }

    fn build_index(&mut self) {
        let blocks = self.bits.len().div_ceil(RANK_BLOCK);
        self.block_prefix = Vec::with_capacity(blocks + 1);
        let mut acc = 0u64;
        for b in 0..blocks {
            self.block_prefix.push(acc);
            let start = b * RANK_BLOCK;
            let end = (start + RANK_BLOCK).min(self.bits.len());
            acc += self.bits.count_range(start, end) as u64;
        }
        self.block_prefix.push(acc);
    }

    /// Number of rows that pass the filter.
    #[inline]
    pub fn visible_rows(&self) -> usize {
        self.visible
    }

    /// Total rows the mask covers.
    #[inline]
    pub fn total_rows(&self) -> usize {
        self.bits.len()
    }

    #[inline]
    pub fn scanned_rows(&self) -> usize {
        self.scanned
    }

    /// True when a row budget stopped the scan early.
    #[inline]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn millis(&self) -> u128 {
        self.millis
    }

    #[inline]
    pub fn is_visible(&self, row: usize) -> bool {
        self.bits.get(row)
    }

    /// Absolute row index of the `n`th visible row, or `None` past the end.
    ///
    /// O(log(rows/4096) + 64) — this is the whole reason the rank index
    /// exists. The renderer calls it once per painted row.
    pub fn nth_visible(&self, n: usize) -> Option<usize> {
        if n >= self.visible {
            return None;
        }
        let n = n as u64;
        // Last block whose prefix is <= n.
        let b = self.block_prefix.partition_point(|&p| p <= n) - 1;
        let mut remaining = n - self.block_prefix[b];
        let start = b * RANK_BLOCK;
        let end = (start + RANK_BLOCK).min(self.bits.len());
        for r in start..end {
            if self.bits.get(r) {
                if remaining == 0 {
                    return Some(r);
                }
                remaining -= 1;
            }
        }
        None
    }

    /// Number of visible rows strictly before `row`. The inverse of
    /// [`RowMask::nth_visible`], used to keep the scroll position stable when
    /// a filter changes.
    pub fn rank(&self, row: usize) -> usize {
        let row = row.min(self.bits.len());
        let b = row / RANK_BLOCK;
        let mut n = self.block_prefix.get(b).copied().unwrap_or(0) as usize;
        n += self.bits.count_range(b * RANK_BLOCK, row);
        n
    }

    /// Materialise the first `limit` visible rows.
    ///
    /// Bounded on purpose, with the same discipline as
    /// [`crate::SearchResults`]: callers that want a list get a capped one and
    /// are told whether it was cut. There is no unbounded variant, because at
    /// 200M rows there must not be.
    pub fn first_visible(&self, limit: usize) -> (Vec<u32>, bool) {
        let mut out = Vec::with_capacity(limit.min(self.visible));
        for r in 0..self.bits.len() {
            if out.len() >= limit {
                return (out, true);
            }
            if self.bits.get(r) {
                out.push(r as u32);
            }
        }
        (out, false)
    }

    /// Intersect with another mask (AND), for combining per-column filters.
    pub fn intersect(&self, other: &RowMask) -> RowMask {
        let len = self.bits.len().max(other.bits.len());
        let mut bits = Bitmap::zeros(len);
        for r in 0..len {
            if self.bits.get(r) && other.bits.get(r) {
                bits.set(r, true);
            }
        }
        RowMask::from_bits(bits)
    }
}

// ===================================================================== table ==

/// One column of a [`Table`].
#[derive(Clone, PartialEq, Debug)]
pub struct TableColumn {
    /// Header caption. Excel requires these to be unique and non-empty within
    /// a table; [`Table::normalise_column_names`] enforces that on the way out.
    pub name: String,
    pub ctype: ColumnType,
    pub validation: Validation,
    pub format: NumberFormat,
    pub conditional: Vec<ConditionalRule>,
    /// Active header filter, if any.
    pub filter: Option<Predicate>,
    /// Excel `totalsRowFunction`, preserved verbatim so a table authored in
    /// Excel keeps its totals row on the way back.
    pub totals_function: Option<String>,
    pub totals_label: Option<String>,
}

impl TableColumn {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ctype: ColumnType::Any,
            validation: Validation::default(),
            format: NumberFormat::General,
            conditional: Vec::new(),
            filter: None,
            totals_function: None,
            totals_label: None,
        }
    }

    pub fn typed(mut self, t: ColumnType) -> Self {
        self.ctype = t;
        self
    }

    pub fn validated(mut self, v: Validation) -> Self {
        self.validation = v;
        self
    }

    pub fn formatted(mut self, f: NumberFormat) -> Self {
        self.format = f;
        self
    }

    pub fn with_conditional(mut self, r: ConditionalRule) -> Self {
        self.conditional.push(r);
        self
    }

    pub fn filtered(mut self, p: Predicate) -> Self {
        self.filter = Some(p);
        self
    }

    /// Does this column need a whole-column pass before a single cell can be
    /// judged?
    pub fn needs_uniqueness(&self) -> bool {
        self.validation.rule == ValidationRule::Unique
    }
}

/// A named, typed, rectangular region of a sheet.
#[derive(Clone, PartialEq, Debug)]
pub struct Table {
    /// Excel `displayName`: letters, digits, underscores, no spaces, must not
    /// look like a cell reference. [`Table::sanitise_name`] coerces.
    pub name: String,
    pub range: TableRange,
    pub columns: Vec<TableColumn>,
    /// Whether `range.first_row` is a header row.
    pub header_row: bool,
    /// Whether `range.last_row` is a totals row.
    pub totals_row: bool,
    pub banded_rows: bool,
    pub banded_cols: bool,
    /// Whether Excel should draw filter dropdowns on the header row.
    pub autofilter: bool,
    /// Excel table style name, e.g. `TableStyleMedium9`. Kept verbatim.
    pub style: Option<String>,
}

impl Table {
    /// A table over `range` with auto-named columns.
    pub fn new(name: impl Into<String>, range: TableRange) -> Self {
        let columns = (0..range.cols())
            .map(|i| TableColumn::new(format!("Column{}", i + 1)))
            .collect();
        Self {
            name: Self::sanitise_name(&name.into()),
            range,
            columns,
            header_row: true,
            totals_row: false,
            banded_rows: true,
            banded_cols: false,
            autofilter: true,
            style: None,
        }
    }

    /// Replace the column definitions, padding or truncating to the range's
    /// width so `columns.len() == range.cols()` always holds.
    pub fn with_columns(mut self, mut columns: Vec<TableColumn>) -> Self {
        let want = self.range.cols();
        while columns.len() < want {
            columns.push(TableColumn::new(format!("Column{}", columns.len() + 1)));
        }
        columns.truncate(want);
        self.columns = columns;
        self.normalise_column_names();
        self
    }

    /// Coerce a string into a legal Excel table name.
    ///
    /// Excel's rules: at least one character, first character a letter,
    /// underscore or backslash, the rest letters/digits/underscores/periods,
    /// no spaces, and it must not be parseable as a cell reference. A name we
    /// cannot fix is replaced rather than rejected, because refusing to name a
    /// table is a worse outcome than renaming one.
    pub fn sanitise_name(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len().max(1));
        for (i, ch) in raw.chars().enumerate() {
            let ok = if i == 0 {
                // Digits are allowed through here and fixed up by the prefix
                // below; replacing them with '_' would silently eat a
                // character out of the user's name.
                ch.is_alphanumeric() || ch == '_'
            } else {
                ch.is_alphanumeric() || ch == '_' || ch == '.'
            };
            out.push(if ok { ch } else { '_' });
        }
        if out.is_empty() {
            return "Table1".to_string();
        }
        if out.starts_with(|c: char| c.is_ascii_digit()) {
            out.insert(0, '_');
        }
        // A bare cell reference like `C4` is illegal as a table name.
        if CellRef::from_a1(&out).is_some() {
            out.push('_');
        }
        out
    }

    /// Make column names non-empty and unique, as Excel requires. Duplicates
    /// get a numeric suffix rather than being dropped.
    pub fn normalise_column_names(&mut self) {
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (i, col) in self.columns.iter_mut().enumerate() {
            if col.name.trim().is_empty() {
                col.name = format!("Column{}", i + 1);
            }
            let key = col.name.to_lowercase();
            let n = seen.entry(key).or_insert(0);
            *n += 1;
            if *n > 1 {
                col.name = format!("{}{}", col.name, *n);
            }
        }
    }

    /// The rows holding data: excludes the header and totals rows.
    pub fn data_rows(&self) -> std::ops::Range<u32> {
        let start = self.range.first_row + u32::from(self.header_row);
        let end = self.range.last_row + 1 - u32::from(self.totals_row);
        start..end.max(start)
    }

    /// Absolute sheet column for table column `i`.
    #[inline]
    pub fn sheet_col(&self, i: usize) -> u32 {
        self.range.first_col + i as u32
    }

    /// Table column index for an absolute sheet column, if inside the table.
    #[inline]
    pub fn column_index(&self, sheet_col: u32) -> Option<usize> {
        (sheet_col >= self.range.first_col && sheet_col <= self.range.last_col)
            .then(|| (sheet_col - self.range.first_col) as usize)
    }

    /// Does `cell` sit in this table's data area?
    pub fn contains_data(&self, cell: CellRef) -> bool {
        self.range.contains(cell) && self.data_rows().contains(&cell.row)
    }

    /// Whether a data row should be painted with the banded (alternate) fill.
    /// Banding is relative to the table, not the sheet, so a table starting on
    /// an even sheet row still bands correctly.
    pub fn is_banded(&self, row: u32) -> bool {
        self.banded_rows && (row - self.data_rows().start) % 2 == 1
    }

    /// Validate one cell against its column's rule.
    ///
    /// This is the renderer's entry point: O(1) apart from the list/regex
    /// scan, and it takes an optional prebuilt [`UniquenessIndex`] so a
    /// `Unique` column does not force a full pass per painted cell.
    ///
    /// Returns `None` when the cell is fine.
    pub fn validate_cell(
        &self,
        col_index: usize,
        value: &Value,
        text: &str,
        unique: Option<&UniquenessIndex>,
    ) -> Option<Violation> {
        let col = self.columns.get(col_index)?;
        let v = &col.validation;

        if value.is_empty() {
            return (!v.allow_empty).then_some(Violation::Empty);
        }
        // An error value fails every typed column: it is not a number, not
        // text, and the user needs to see it.
        if let Value::Error(e) = value {
            if col.ctype != ColumnType::Any {
                return Some(Violation::ErrorValue(*e));
            }
        }
        if !col.ctype.accepts(value) {
            return Some(Violation::WrongType(col.ctype));
        }

        let num = match value {
            Value::Number(n) => Some(*n),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        };

        match &v.rule {
            ValidationRule::None => None,
            ValidationRule::Between { min, max } => match num {
                Some(n) if n >= *min && n <= *max => None,
                Some(_) => Some(Violation::OutOfRange {
                    min: *min,
                    max: *max,
                }),
                // A non-numeric cell in a range-bounded column is a type
                // problem, reported as such rather than as "out of range".
                None => Some(Violation::WrongType(ColumnType::Number)),
            },
            ValidationRule::NotBetween { min, max } => match num {
                Some(n) if n < *min || n > *max => None,
                Some(_) => Some(Violation::OutOfRange {
                    min: *min,
                    max: *max,
                }),
                None => Some(Violation::WrongType(ColumnType::Number)),
            },
            ValidationRule::Compare { op, value: rhs } => match num {
                Some(n) if op.test(n, *rhs) => None,
                Some(_) => Some(Violation::FailsCompare {
                    op: *op,
                    value: *rhs,
                }),
                None => Some(Violation::WrongType(ColumnType::Number)),
            },
            ValidationRule::OneOf(list) => list
                .iter()
                .any(|a| a.eq_ignore_ascii_case(text))
                .then_some(())
                .map_or(Some(Violation::NotInList), |_| None),
            ValidationRule::Regex(pat) => match regex_lite::Regex::new(&anchored(pat)) {
                // An unparseable pattern must not condemn every cell in the
                // column; a broken rule is the rule's problem, not the data's.
                Err(_) => None,
                Ok(re) => (!re.is_match(text)).then_some(Violation::RegexMismatch),
            },
            ValidationRule::TextLength { min, max } => {
                let got = text.chars().count() as u32;
                (got < *min || got > *max).then_some(Violation::BadLength {
                    min: *min,
                    max: *max,
                    got,
                })
            }
            ValidationRule::Unique => unique
                .is_some_and(|u| u.is_duplicate(value))
                .then_some(Violation::Duplicate),
        }
    }
}

/// Anchor a user-supplied pattern to the whole cell, matching how Excel's
/// list/length validations behave (and how people expect a validation regex to
/// read). An already-anchored pattern is left alone.
fn anchored(pat: &str) -> String {
    let has_start = pat.starts_with('^');
    let has_end = pat.ends_with('$') && !pat.ends_with("\\$");
    match (has_start, has_end) {
        (true, true) => pat.to_string(),
        (true, false) => format!("{pat}$"),
        (false, true) => format!("^{pat}"),
        (false, false) => format!("^(?:{pat})$"),
    }
}

#[cfg(test)]
mod tests;
