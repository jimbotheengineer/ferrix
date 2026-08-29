//! Sheet-range data validation (issue #41).
//!
//! ## Why this exists when `table::Validation` already does
//!
//! [`crate::table::Validation`] is scoped to a STRUCTURED TABLE COLUMN. It is
//! reached through `Table::columns[i].validation`, so it can only describe a
//! rule on a column of a defined Excel Table. Issue #41 asks for validation on
//! an arbitrary sheet RANGE — `B2:B500` on a sheet with no table on it at all.
//! That is genuinely new storage, and this module is it.
//!
//! Everything *below* the storage is reused rather than reinvented:
//! [`ValidationRule`] is the same predicate enum the table columns use, and
//! [`Violation`] is the same explanation type, so `table_xlsx`'s export helper
//! and the grid's red-triangle flag keep working unchanged and there is
//! exactly one place that knows what "between 1 and 10" means.
//!
//! ## Scope, and why a rule is never per cell
//!
//! One [`RangeValidation`] entry covers a whole rectangle, the same discipline
//! `SheetFormat` uses for conditional rules. A "whole number 1..100" rule over
//! `B2:B200000000` is ONE entry of a few dozen bytes. There is no per-cell
//! store and no per-cell index, so [`SheetValidation::heap_bytes`] is a
//! function of how many rules exist and never of how many rows they cover —
//! `rule_over_200m_rows_stores_one_entry` asserts exactly that.
//!
//! Lookup is a linear walk of the rule list, LAST match winning, which is
//! O(rules) and bounded by [`MAX_RULES`]. It is deliberately not accelerated
//! by an interval tree: at a few thousand rules the walk is nanoseconds, and a
//! second index keyed by cell is precisely the thing the scale invariant
//! forbids.
//!
//! ## What a check needs, and why it is not a `Value`
//!
//! A cell being PAINTED has a [`Value`]; a cell being TYPED INTO has only the
//! raw string the user is part-way through entering, and interning it into the
//! arena just to type-check it would grow the arena on every keystroke. So the
//! checker takes a [`Candidate`] — a number if it looks like one, the display
//! text, and an error kind — which both sides can produce cheaply.

use crate::table::{ColumnType, ValidationRule, Violation};
use crate::{CellRef, ErrorKind, TableRange, Value};

/// Upper bound on rules per sheet.
///
/// A cap rather than an unbounded `Vec` because every rule is walked on every
/// validated cell; the number exists so a pathological import cannot turn the
/// paint loop into an O(n²) walk. Well past anything a human authors.
pub const MAX_RULES: usize = 4096;

/// The lowest serial number Ferrix treats as a date (1900-01-01).
pub const DATE_SERIAL_MIN: f64 = 1.0;
/// The highest serial number Ferrix treats as a date (9999-12-31).
pub const DATE_SERIAL_MAX: f64 = 2_958_465.0;

// ================================================================== domain ==

/// The kind of value a range accepts — Excel's `<dataValidation type="...">`.
///
/// Separate from [`ValidationRule`] because the two answer different
/// questions. The domain says *what kind of thing this is* (a whole number, a
/// date, one of a list); the rule says *which ones of that kind are allowed*
/// (between 1 and 10). Excel splits them the same way, which is why the xlsx
/// round trip is a straight mapping rather than a re-derivation.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ValueDomain {
    /// Anything at all. The rule, if any, still applies.
    #[default]
    Any,
    /// An integer. `2.5` fails even when the bounds would admit it.
    WholeNumber,
    /// Any number.
    Decimal,
    /// A number in the serial-date range. Ferrix has no date type, so a date
    /// is an f64 and this is the check that says so.
    Date,
    /// One of an explicit list of strings. Renders an in-cell dropdown.
    List,
    /// The display text's character count is what the rule bounds.
    TextLength,
    /// A user formula that must evaluate truthy for the cell to pass.
    Custom,
}

impl ValueDomain {
    pub const ALL: [ValueDomain; 7] = [
        ValueDomain::Any,
        ValueDomain::WholeNumber,
        ValueDomain::Decimal,
        ValueDomain::Date,
        ValueDomain::List,
        ValueDomain::TextLength,
        ValueDomain::Custom,
    ];

    /// Label for a menu or a dialog.
    pub fn label(self) -> &'static str {
        match self {
            ValueDomain::Any => "Any value",
            ValueDomain::WholeNumber => "Whole number",
            ValueDomain::Decimal => "Decimal",
            ValueDomain::Date => "Date",
            ValueDomain::List => "List",
            ValueDomain::TextLength => "Text length",
            ValueDomain::Custom => "Custom formula",
        }
    }

    /// The `type` attribute Excel writes.
    pub fn as_xlsx(self) -> &'static str {
        match self {
            ValueDomain::Any => "none",
            ValueDomain::WholeNumber => "whole",
            ValueDomain::Decimal => "decimal",
            ValueDomain::Date => "date",
            ValueDomain::List => "list",
            ValueDomain::TextLength => "textLength",
            ValueDomain::Custom => "custom",
        }
    }

    pub fn from_xlsx(s: &str) -> Option<Self> {
        Some(match s {
            "none" => ValueDomain::Any,
            "whole" => ValueDomain::WholeNumber,
            "decimal" => ValueDomain::Decimal,
            // Ferrix stores dates as serial numbers, so `time` lands in the
            // same place `date` does rather than being dropped.
            "date" | "time" => ValueDomain::Date,
            "list" => ValueDomain::List,
            "textLength" => ValueDomain::TextLength,
            "custom" => ValueDomain::Custom,
            _ => return None,
        })
    }

    /// Does this domain require the cell to hold a number?
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            ValueDomain::WholeNumber | ValueDomain::Decimal | ValueDomain::Date
        )
    }
}

// ============================================================= error style ==

/// What happens when an entry fails.
///
/// The distinction the acceptance criterion names: `Stop` REJECTS the edit,
/// `Warning` and `Information` let it through and say so. Nothing here decides
/// that on its own — [`ErrorStyle::rejects`] is the single predicate every
/// caller asks, so the two paths cannot drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ErrorStyle {
    /// Refuse the entry. The cell keeps the value it had.
    #[default]
    Stop,
    /// Accept the entry, but tell the user it broke the rule.
    Warning,
    /// Accept the entry with a quieter note.
    Information,
}

impl ErrorStyle {
    pub const ALL: [ErrorStyle; 3] = [
        ErrorStyle::Stop,
        ErrorStyle::Warning,
        ErrorStyle::Information,
    ];

    /// THE predicate. `true` means the edit must not be written.
    pub fn rejects(self) -> bool {
        matches!(self, ErrorStyle::Stop)
    }

    pub fn label(self) -> &'static str {
        match self {
            ErrorStyle::Stop => "Stop (reject the entry)",
            ErrorStyle::Warning => "Warning (allow, with a warning)",
            ErrorStyle::Information => "Information (allow, with a note)",
        }
    }

    pub fn as_xlsx(self) -> &'static str {
        match self {
            ErrorStyle::Stop => "stop",
            ErrorStyle::Warning => "warning",
            ErrorStyle::Information => "information",
        }
    }

    pub fn from_xlsx(s: &str) -> Option<Self> {
        Some(match s {
            "stop" => ErrorStyle::Stop,
            "warning" => ErrorStyle::Warning,
            "information" => ErrorStyle::Information,
            _ => return None,
        })
    }
}

// =============================================================== candidate ==

/// A value about to be checked, in the shape both callers can produce.
///
/// The paint loop has a [`Value`] and its display text. The edit path has only
/// what the user typed, and must not intern it — a validation check that grew
/// the string arena by one entry per keystroke would make typing allocate
/// permanently. So the checker takes this instead of a `Value`.
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
    /// The numeric reading, when there is one. `TRUE`/`FALSE` read as 1/0,
    /// matching [`crate::table::Table::validate_cell`].
    pub num: Option<f64>,
    /// Display text, which is what list/length/regex rules compare against.
    pub text: &'a str,
    /// Set when the cell holds a spreadsheet error value.
    pub error: Option<ErrorKind>,
    /// True when there is nothing in the cell at all.
    pub empty: bool,
}

impl<'a> Candidate<'a> {
    /// From a stored cell value plus its already-resolved display text.
    pub fn from_value(value: &Value, text: &'a str) -> Self {
        Self {
            num: match value {
                Value::Number(n) => Some(*n),
                Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
                _ => None,
            },
            text,
            error: value.error(),
            empty: value.is_empty(),
        }
    }

    /// From raw typed input, WITHOUT interning it.
    ///
    /// Reads the same three shapes `Workbook::classify` does — number, boolean
    /// literal, text — so "what validation checked" and "what got stored" can
    /// not disagree about whether `007` is a number.
    pub fn from_input(raw: &'a str) -> Self {
        let t = raw.trim();
        if t.is_empty() {
            return Self {
                num: None,
                text: t,
                error: None,
                empty: true,
            };
        }
        let num = t
            .parse::<f64>()
            .ok()
            .or_else(|| match t.to_ascii_uppercase().as_str() {
                "TRUE" => Some(1.0),
                "FALSE" => Some(0.0),
                _ => None,
            });
        Self {
            num,
            text: t,
            error: None,
            empty: false,
        }
    }
}

// ========================================================= range validation ==

/// One validation rule, attached to one rectangle.
///
/// ONE entry however many cells the rectangle spans.
#[derive(Clone, PartialEq, Debug)]
pub struct RangeValidation {
    pub range: TableRange,
    pub domain: ValueDomain,
    /// The predicate within the domain. Reused from the table model rather
    /// than duplicated, so `Between`/`OneOf`/`TextLength` mean one thing in
    /// this codebase.
    pub rule: ValidationRule,
    /// Empty cells pass regardless. Excel's `allowBlank`.
    pub allow_empty: bool,
    /// The sentence shown when an entry fails. This is the "custom message"
    /// the acceptance criterion asks for; `None` falls back to
    /// [`Violation::describe`].
    pub message: Option<String>,
    /// Title of the error box, exported as `errorTitle`.
    pub title: Option<String>,
    pub style: ErrorStyle,
    /// Draw the in-cell dropdown for a [`ValueDomain::List`] rule.
    pub show_dropdown: bool,
}

impl RangeValidation {
    pub fn new(range: TableRange, domain: ValueDomain, rule: ValidationRule) -> Self {
        Self {
            range,
            domain,
            rule,
            allow_empty: true,
            message: None,
            title: None,
            style: ErrorStyle::Stop,
            show_dropdown: true,
        }
    }

    /// A list rule over `values`, the shape that renders a dropdown.
    pub fn list(range: TableRange, values: Vec<String>) -> Self {
        Self::new(range, ValueDomain::List, ValidationRule::OneOf(values))
    }

    pub fn with_message(mut self, m: impl Into<String>) -> Self {
        self.message = Some(m.into());
        self
    }

    pub fn with_title(mut self, t: impl Into<String>) -> Self {
        self.title = Some(t.into());
        self
    }

    pub fn with_style(mut self, s: ErrorStyle) -> Self {
        self.style = s;
        self
    }

    pub fn with_allow_empty(mut self, yes: bool) -> Self {
        self.allow_empty = yes;
        self
    }

    pub fn with_dropdown(mut self, yes: bool) -> Self {
        self.show_dropdown = yes;
        self
    }

    /// The values a [`ValueDomain::List`] rule offers, or `None` when this is
    /// not a list rule. THE accessor the dropdown is drawn from.
    pub fn list_values(&self) -> Option<&[String]> {
        match (&self.domain, &self.rule) {
            (ValueDomain::List, ValidationRule::OneOf(v)) => Some(v),
            _ => None,
        }
    }

    /// The formula a [`ValueDomain::Custom`] rule must satisfy.
    pub fn custom_formula(&self) -> Option<&str> {
        match (&self.domain, &self.rule) {
            (ValueDomain::Custom, ValidationRule::CustomFormula(f)) => Some(f.as_str()),
            _ => None,
        }
    }

    /// Check one candidate. `None` means it passes.
    ///
    /// `custom` is the evaluated truth of a [`ValueDomain::Custom`] rule's
    /// formula, supplied by whoever owns an evaluator. `None` means "not
    /// evaluated here", and an unevaluated custom rule PASSES rather than
    /// condemning every cell — the same stance `validate_cell` takes for an
    /// unparseable regex: a rule that cannot be run is the rule's problem.
    pub fn check(&self, c: &Candidate<'_>, custom: Option<bool>) -> Option<Violation> {
        if c.empty {
            return (!self.allow_empty).then_some(Violation::Empty);
        }
        if let Some(e) = c.error {
            return Some(Violation::ErrorValue(e));
        }

        // --- the domain gate ---
        match self.domain {
            ValueDomain::Any | ValueDomain::TextLength | ValueDomain::List => {}
            ValueDomain::Decimal => {
                if c.num.is_none() {
                    return Some(Violation::WrongType(ColumnType::Number));
                }
            }
            ValueDomain::WholeNumber => match c.num {
                None => return Some(Violation::WrongType(ColumnType::Number)),
                Some(n) if n.fract() != 0.0 => return Some(Violation::NotWhole),
                Some(_) => {}
            },
            ValueDomain::Date => match c.num {
                None => return Some(Violation::NotADate),
                Some(n) if !(DATE_SERIAL_MIN..=DATE_SERIAL_MAX).contains(&n) => {
                    return Some(Violation::NotADate)
                }
                Some(_) => {}
            },
            ValueDomain::Custom => {
                if custom == Some(false) {
                    return Some(Violation::CustomFailed);
                }
                // A custom rule's whole meaning is its formula, so there is no
                // second predicate to fall through to.
                return None;
            }
        }

        // --- the predicate within the domain ---
        match &self.rule {
            ValidationRule::None | ValidationRule::CustomFormula(_) => None,
            ValidationRule::Between { min, max } => match c.num {
                Some(n) if n >= *min && n <= *max => None,
                Some(_) => Some(Violation::OutOfRange {
                    min: *min,
                    max: *max,
                }),
                None => Some(Violation::WrongType(ColumnType::Number)),
            },
            ValidationRule::NotBetween { min, max } => match c.num {
                Some(n) if n < *min || n > *max => None,
                Some(_) => Some(Violation::OutOfRange {
                    min: *min,
                    max: *max,
                }),
                None => Some(Violation::WrongType(ColumnType::Number)),
            },
            ValidationRule::Compare { op, value } => match c.num {
                Some(n) if op.test(n, *value) => None,
                Some(_) => Some(Violation::FailsCompare {
                    op: *op,
                    value: *value,
                }),
                None => Some(Violation::WrongType(ColumnType::Number)),
            },
            ValidationRule::OneOf(list) => (!list.iter().any(|a| a.eq_ignore_ascii_case(c.text)))
                .then_some(Violation::NotInList),
            ValidationRule::Regex(pat) => {
                match regex_lite::Regex::new(&crate::table::anchored(pat)) {
                    Err(_) => None,
                    Ok(re) => (!re.is_match(c.text)).then_some(Violation::RegexMismatch),
                }
            }
            ValidationRule::TextLength { min, max } => {
                let got = c.text.chars().count() as u32;
                (got < *min || got > *max).then_some(Violation::BadLength {
                    min: *min,
                    max: *max,
                    got,
                })
            }
            // Uniqueness needs a whole-column index. At table scope that is
            // affordable because a table is bounded; at SHEET scope the range
            // may be 200M rows, and building a counter per distinct value on
            // every keystroke is exactly the per-row cost the scale invariant
            // forbids. So it is not offered here — `ValueDomain` has no
            // "unique" member and this arm is unreachable from the editor.
            ValidationRule::Unique => None,
        }
    }

    /// The sentence to show the user for a violation of THIS rule.
    ///
    /// The custom message wins when there is one; that is the whole point of
    /// storing it. Otherwise the violation explains itself.
    pub fn explain(&self, v: &Violation) -> String {
        match &self.message {
            Some(m) if !m.trim().is_empty() => m.clone(),
            _ => v.describe(),
        }
    }

    pub fn heap_bytes(&self) -> usize {
        let rule = match &self.rule {
            ValidationRule::OneOf(v) => {
                v.capacity() * std::mem::size_of::<String>()
                    + v.iter().map(String::capacity).sum::<usize>()
            }
            ValidationRule::Regex(s) | ValidationRule::CustomFormula(s) => s.capacity(),
            _ => 0,
        };
        std::mem::size_of::<Self>()
            + rule
            + self.message.as_ref().map_or(0, String::capacity)
            + self.title.as_ref().map_or(0, String::capacity)
    }
}

// ========================================================= sheet validation ==

/// Every validation rule on one sheet, keyed by RANGE.
///
/// The store the acceptance criterion means by "stored per range, like
/// conditional rules". Modelled on `SheetFormat::ranges`: a `Vec` in
/// user-visible order, later entries winning, so "the rule on this cell" has a
/// single deterministic answer and the Manage list can be drawn in storage
/// order.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SheetValidation {
    rules: Vec<RangeValidation>,
}

impl SheetValidation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn rules(&self) -> &[RangeValidation] {
        &self.rules
    }

    pub fn get(&self, i: usize) -> Option<&RangeValidation> {
        self.rules.get(i)
    }

    /// Bytes of heap this store owns.
    ///
    /// Public because it is the number the scale test asserts on: a function
    /// of the rule COUNT, never of the row count they cover.
    pub fn heap_bytes(&self) -> usize {
        self.rules.iter().map(RangeValidation::heap_bytes).sum()
    }

    /// Append a rule. Returns its index, or `None` when [`MAX_RULES`] is hit.
    pub fn push(&mut self, rule: RangeValidation) -> Option<usize> {
        if self.rules.len() >= MAX_RULES {
            return None;
        }
        self.rules.push(rule);
        Some(self.rules.len() - 1)
    }

    /// Replace the rule at `i`, keeping its precedence position.
    pub fn set(&mut self, i: usize, rule: RangeValidation) -> bool {
        match self.rules.get_mut(i) {
            Some(slot) => {
                *slot = rule;
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, i: usize) -> Option<RangeValidation> {
        (i < self.rules.len()).then(|| self.rules.remove(i))
    }

    /// Drop every rule that covers any part of `range`.
    ///
    /// The "Clear validation from the selection" gesture. Whole entries go:
    /// splitting a rectangle around a cleared sub-rectangle would turn one
    /// entry into up to four, which is how a per-range store degenerates into
    /// a per-cell one.
    pub fn clear_overlapping(&mut self, range: TableRange) -> usize {
        let before = self.rules.len();
        self.rules.retain(|r| !overlaps(r.range, range));
        before - self.rules.len()
    }

    pub fn clear(&mut self) {
        self.rules.clear();
    }

    /// Index of the entry on exactly `range`, if one exists.
    pub fn index_of_range(&self, range: TableRange) -> Option<usize> {
        self.rules.iter().position(|r| r.range == range)
    }

    /// The rule governing `cell`, with its index. LAST match wins.
    ///
    /// O(rules), which is bounded by [`MAX_RULES`] and independent of the row
    /// count. This is the hot path — the paint loop calls it once per VISIBLE
    /// cell — and it allocates nothing.
    pub fn rule_for(&self, cell: CellRef) -> Option<(usize, &RangeValidation)> {
        self.rules
            .iter()
            .enumerate()
            .rev()
            .find(|(_, r)| r.range.contains(cell))
    }

    /// Check one cell against whatever rule governs it.
    pub fn check_cell(
        &self,
        cell: CellRef,
        c: &Candidate<'_>,
        custom: Option<bool>,
    ) -> Option<(usize, Violation)> {
        let (i, rule) = self.rule_for(cell)?;
        rule.check(c, custom).map(|v| (i, v))
    }

    /// The list values offered in `cell`, when a list rule governs it and asks
    /// for a dropdown. THE accessor both the grid marker and the popup read,
    /// so a dropdown cannot be painted where there is nothing to pick.
    pub fn dropdown_for(&self, cell: CellRef) -> Option<&[String]> {
        let (_, rule) = self.rule_for(cell)?;
        rule.show_dropdown.then(|| rule.list_values()).flatten()
    }
}

/// Do two rectangles share any cell?
fn overlaps(a: TableRange, b: TableRange) -> bool {
    a.first_row <= b.last_row
        && b.first_row <= a.last_row
        && a.first_col <= b.last_col
        && b.first_col <= a.last_col
}

#[cfg(test)]
mod tests;
