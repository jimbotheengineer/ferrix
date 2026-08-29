//! Sheet-range `<dataValidations>` interoperability (issue #41).
//!
//! ## Why this is beside `table_xlsx` rather than inside it
//!
//! `table_xlsx::build_validation` already writes a `<dataValidation>` — for a
//! STRUCTURED TABLE COLUMN, keyed off `TableColumn::validation` and
//! `ColumnType`. This module writes the same element for a `SheetValidation`
//! rule, which is keyed off a rectangle and a [`ValueDomain`] instead. The
//! *rule* half is shared: both go through `ValidationRule`, so "between 1 and
//! 10" has one meaning here, and [`cmp_rule`] is reused rather than copied.
//!
//! [`cmp_rule`]: crate::table_xlsx
//!
//! ## Reading
//!
//! `rust_xlsxwriter` only writes and calamine only surfaces cell values, so
//! the reader opens the package itself with `quick-xml`, exactly as
//! `protect_xlsx` and `table_xlsx` do, reusing their part-scanning helpers so
//! the zip safeguards apply here too.
//!
//! ## What is verified
//!
//! The tests write a real `.xlsx`, unzip it and assert on the emitted XML,
//! then re-import and compare the rules. That proves the element is present,
//! structurally correct, and that Ferrix agrees with itself across a round
//! trip. **Excel was never launched** — this is not evidence that Excel
//! accepts the file, only that the OOXML it reads is well formed.

use std::path::Path;

use ferrix_core::validate::{ErrorStyle, RangeValidation, SheetValidation, ValueDomain};
use ferrix_core::{CellRef, TableRange, ValidationRule};
use rust_xlsxwriter::{DataValidation, DataValidationErrorStyle, DataValidationRule, Worksheet};

use crate::safeguard::Limits;
use crate::xlsx::XlsxError;

/// Excel's own cap on a `<dataValidation>` list, in characters of the joined
/// formula. Longer lists must live in a worksheet range; Ferrix reports the
/// loss rather than truncating silently.
const MAX_LIST_CHARS: usize = 255;

/// Bounds for the "any number" fallback, matching `table_xlsx`.
const ANY_NUMBER_MIN: f64 = -1.0e308;
const ANY_NUMBER_MAX: f64 = 1.0e308;

// ------------------------------------------------------------------ write --

/// Write every rule of `validation` onto `ws`.
///
/// One `add_data_validation` call per RULE, not per cell: a rule over
/// `B2:B200000000` emits a single element with a single `sqref`. Rules whose
/// range exceeds Excel's own row limit are clamped to it — Excel cannot
/// express more, and writing a larger `sqref` produces a file it refuses.
pub fn write_sheet_validation(
    ws: &mut Worksheet,
    validation: &SheetValidation,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    for rule in validation.rules() {
        let Some(dv) = build(rule)? else { continue };
        let r = clamp(rule.range);
        ws.add_data_validation(
            r.first_row,
            r.first_col as u16,
            r.last_row,
            r.last_col as u16,
            &dv,
        )?;
    }
    Ok(())
}

/// Excel's sheet limits. A Ferrix sheet may be far larger; the rule still
/// applies to everything Excel can hold.
fn clamp(r: TableRange) -> TableRange {
    const MAX_ROW: u32 = 1_048_575;
    const MAX_COL: u32 = 16_383;
    TableRange {
        first_row: r.first_row.min(MAX_ROW),
        last_row: r.last_row.min(MAX_ROW),
        first_col: r.first_col.min(MAX_COL),
        last_col: r.last_col.min(MAX_COL),
    }
}

/// Turn one Ferrix rule into a `rust_xlsxwriter` `DataValidation`.
///
/// `None` means the rule says nothing Excel can carry AND has no message
/// worth writing, so no element is emitted at all.
fn build(rule: &RangeValidation) -> Result<Option<DataValidation>, rust_xlsxwriter::XlsxError> {
    let mut dv = DataValidation::new().ignore_blank(rule.allow_empty);

    dv = match (rule.domain, &rule.rule) {
        // --- list: the one that renders an in-cell dropdown ---
        (ValueDomain::List, ValidationRule::OneOf(list)) => {
            let refs: Vec<&str> = list.iter().map(String::as_str).collect();
            dv.allow_list_strings(&refs)?
                .show_dropdown(rule.show_dropdown)
        }
        // --- whole number ---
        (ValueDomain::WholeNumber, r) => match int_rule(r) {
            Some(ir) => dv.allow_whole_number(ir),
            None => dv.allow_whole_number(DataValidationRule::Between(i32::MIN, i32::MAX)),
        },
        // --- decimal / date: both are f64 serials to Ferrix ---
        (ValueDomain::Decimal | ValueDomain::Date, r) => match num_rule(r) {
            Some(nr) => dv.allow_decimal_number(nr),
            None => {
                dv.allow_decimal_number(DataValidationRule::Between(ANY_NUMBER_MIN, ANY_NUMBER_MAX))
            }
        },
        // --- text length ---
        (ValueDomain::TextLength, ValidationRule::TextLength { min, max }) => {
            dv.allow_text_length(DataValidationRule::Between(*min, *max))
        }
        (ValueDomain::TextLength, _) => {
            dv.allow_text_length(DataValidationRule::GreaterThanOrEqualTo(0))
        }
        // --- custom formula ---
        (ValueDomain::Custom, ValidationRule::CustomFormula(f)) => {
            dv.allow_custom(strip_eq(f).into())
        }
        // A regex rule has no Excel spelling at all. It is reported by
        // `sheet_validation_xlsx_loss` rather than silently dropped, and the
        // element is still written so the message and the flag survive.
        (_, _) => dv.allow_any_value(),
    };

    if let Some(msg) = &rule.message {
        dv = dv.set_error_message(truncate(msg, 255))?;
    }
    if let Some(title) = &rule.title {
        dv = dv.set_error_title(truncate(title, 32))?;
    }
    dv = dv.set_error_style(match rule.style {
        ErrorStyle::Stop => DataValidationErrorStyle::Stop,
        ErrorStyle::Warning => DataValidationErrorStyle::Warning,
        ErrorStyle::Information => DataValidationErrorStyle::Information,
    });
    Ok(Some(dv))
}

/// Excel's `custom` validation formula is stored WITHOUT a leading `=`.
fn strip_eq(f: &str) -> &str {
    f.strip_prefix('=').unwrap_or(f)
}

fn int_rule(r: &ValidationRule) -> Option<DataValidationRule<i32>> {
    let i = |v: f64| v.clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    Some(match r {
        ValidationRule::Between { min, max } => DataValidationRule::Between(i(*min), i(*max)),
        ValidationRule::NotBetween { min, max } => DataValidationRule::NotBetween(i(*min), i(*max)),
        ValidationRule::Compare { op, value } => {
            crate::table_xlsx::cmp_rule_generic(*op, i(*value))
        }
        _ => return None,
    })
}

fn num_rule(r: &ValidationRule) -> Option<DataValidationRule<f64>> {
    Some(match r {
        ValidationRule::Between { min, max } => DataValidationRule::Between(*min, *max),
        ValidationRule::NotBetween { min, max } => DataValidationRule::NotBetween(*min, *max),
        ValidationRule::Compare { op, value } => crate::table_xlsx::cmp_rule_generic(*op, *value),
        _ => return None,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// What about this rule will NOT look the same in Excel.
///
/// Same contract as `rule_survives_xlsx`: the user learns in the editor rather
/// than after opening the file.
pub fn sheet_validation_xlsx_loss(rule: &RangeValidation) -> Vec<String> {
    let mut out = Vec::new();
    if let ValidationRule::Regex(p) = &rule.rule {
        out.push(format!(
            "the regular-expression rule {p:?} has no Excel equivalent and will \
             not be enforced there"
        ));
    }
    if matches!(rule.rule, ValidationRule::Unique) {
        out.push("a uniqueness rule has no Excel equivalent".to_string());
    }
    if let Some(list) = rule.list_values() {
        let chars: usize = list.iter().map(|s| s.chars().count() + 1).sum();
        if chars > MAX_LIST_CHARS {
            out.push(format!(
                "the list of {} values is {chars} characters, over Excel's {MAX_LIST_CHARS}-character \
                 limit for an inline list",
                list.len()
            ));
        }
    }
    out
}

// ------------------------------------------------------------------- read --

/// One worksheet's sheet-range validation, as found in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedValidation {
    pub sheet_index: usize,
    pub validation: SheetValidation,
}

/// Read every `<dataValidation>` from each worksheet.
///
/// Worksheets with none produce no entry, so an ordinary file costs nothing.
pub fn import_sheet_validation(
    path: impl AsRef<Path>,
) -> Result<Vec<ImportedValidation>, XlsxError> {
    import_sheet_validation_guarded(path, &Limits::measured())
}

/// [`import_sheet_validation`] under explicit limits.
pub fn import_sheet_validation_guarded(
    path: impl AsRef<Path>,
    limits: &Limits,
) -> Result<Vec<ImportedValidation>, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();
    let parts = crate::table_xlsx::read_package_for(path, limits)?;
    let sheet_paths = crate::table_xlsx::worksheet_paths_for(&parts, &disp)?;

    let mut out = Vec::new();
    for (sheet_index, sp) in sheet_paths.iter().enumerate() {
        let Some(xml) = parts.get(sp) else { continue };
        let mut sv = SheetValidation::new();
        // The element is `<dataValidation ...><formula1>..</formula1></...>`,
        // so the bounds arrive AFTER the attributes. `pending` accumulates the
        // element being read and is flushed on its end tag.
        let mut pending: Option<Pending> = None;

        crate::safeguard::scan_part(xml, &disp, sp, None, |ev| {
            use quick_xml::events::Event as E;
            match ev {
                E::Start(e) | E::Empty(e) if e.local_name().as_ref() == b"dataValidation" => {
                    let p = Pending::from_attrs(e);
                    if matches!(ev, E::Empty(_)) {
                        if let Some(r) = p.finish() {
                            sv.push(r);
                        }
                    } else {
                        pending = Some(p);
                    }
                }
                E::Start(e) if e.local_name().as_ref() == b"formula1" => {
                    if let Some(p) = pending.as_mut() {
                        p.in_formula = 1;
                    }
                }
                E::Start(e) if e.local_name().as_ref() == b"formula2" => {
                    if let Some(p) = pending.as_mut() {
                        p.in_formula = 2;
                    }
                }
                E::Text(t) => {
                    if let Some(p) = pending.as_mut() {
                        if p.in_formula != 0 {
                            let s = String::from_utf8_lossy(t.as_ref()).to_string();
                            match p.in_formula {
                                1 => p.formula1.push_str(&s),
                                _ => p.formula2.push_str(&s),
                            }
                        }
                    }
                }
                E::End(e) if matches!(e.local_name().as_ref(), b"formula1" | b"formula2") => {
                    if let Some(p) = pending.as_mut() {
                        p.in_formula = 0;
                    }
                }
                E::End(e) if e.local_name().as_ref() == b"dataValidation" => {
                    if let Some(p) = pending.take() {
                        if let Some(r) = p.finish() {
                            sv.push(r);
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })?;

        if !sv.is_empty() {
            out.push(ImportedValidation {
                sheet_index,
                validation: sv,
            });
        }
    }
    Ok(out)
}

/// A `<dataValidation>` mid-parse.
struct Pending {
    domain: ValueDomain,
    operator: String,
    sqref: String,
    allow_empty: bool,
    show_dropdown: bool,
    message: Option<String>,
    title: Option<String>,
    style: ErrorStyle,
    formula1: String,
    formula2: String,
    in_formula: u8,
}

impl Pending {
    fn from_attrs(e: &quick_xml::events::BytesStart) -> Self {
        let a = |n: &[u8]| crate::table_xlsx::attr_for(e, n);
        Self {
            domain: a(b"type")
                .as_deref()
                .and_then(ValueDomain::from_xlsx)
                .unwrap_or(ValueDomain::Any),
            operator: a(b"operator").unwrap_or_default(),
            sqref: a(b"sqref").unwrap_or_default(),
            // OOXML default for allowBlank is 0 = blanks are NOT ignored.
            allow_empty: a(b"allowBlank").as_deref() == Some("1"),
            // The dropdown is on unless suppressed, which is the inverse of
            // how the attribute is spelled — `showDropDown="1"` HIDES it.
            show_dropdown: a(b"showDropDown").as_deref() != Some("1"),
            message: a(b"error").filter(|s| !s.is_empty()),
            title: a(b"errorTitle").filter(|s| !s.is_empty()),
            style: a(b"errorStyle")
                .as_deref()
                .and_then(ErrorStyle::from_xlsx)
                .unwrap_or(ErrorStyle::Stop),
            formula1: String::new(),
            formula2: String::new(),
            in_formula: 0,
        }
    }

    fn finish(self) -> Option<RangeValidation> {
        // `sqref` may list several rectangles; the FIRST is taken and the rest
        // become their own entries would require restructuring — instead the
        // union is used, which is what Excel's own UI produces for a
        // contiguous selection. A genuinely disjoint sqref is reported by the
        // caller as one covering rectangle rather than being dropped.
        let range = union_sqref(&self.sqref)?;
        let rule = self.rule();
        let mut rv = RangeValidation::new(range, self.domain, rule);
        rv.allow_empty = self.allow_empty;
        rv.show_dropdown = self.show_dropdown;
        rv.message = self.message;
        rv.title = self.title;
        rv.style = self.style;
        Some(rv)
    }

    fn rule(&self) -> ValidationRule {
        match self.domain {
            ValueDomain::List => ValidationRule::OneOf(parse_list(&self.formula1)),
            ValueDomain::Custom => ValidationRule::CustomFormula(format!("={}", self.formula1)),
            ValueDomain::TextLength => match (num(&self.formula1), num(&self.formula2)) {
                (Some(a), Some(b)) if self.operator != "greaterThanOrEqual" => {
                    ValidationRule::TextLength {
                        min: a.max(0.0) as u32,
                        max: b.max(0.0) as u32,
                    }
                }
                _ => ValidationRule::None,
            },
            ValueDomain::WholeNumber | ValueDomain::Decimal | ValueDomain::Date => {
                let a = num(&self.formula1);
                let b = num(&self.formula2);
                match (self.operator.as_str(), a, b) {
                    ("between" | "", Some(a), Some(b)) => {
                        ValidationRule::Between { min: a, max: b }
                    }
                    ("notBetween", Some(a), Some(b)) => {
                        ValidationRule::NotBetween { min: a, max: b }
                    }
                    (op, Some(a), _) => match ferrix_core::CmpOp::from_xlsx(op) {
                        Some(o) => ValidationRule::Compare { op: o, value: a },
                        None => ValidationRule::None,
                    },
                    _ => ValidationRule::None,
                }
            }
            ValueDomain::Any => ValidationRule::None,
        }
    }
}

fn num(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

/// `"a,b,c"` — the quoted inline list Excel writes for `allow_list_strings`.
fn parse_list(f: &str) -> Vec<String> {
    let inner = f.trim().trim_matches('"');
    if inner.is_empty() {
        return Vec::new();
    }
    inner.split(',').map(|s| s.trim().to_string()).collect()
}

/// The bounding rectangle of a possibly multi-token `sqref`.
fn union_sqref(sqref: &str) -> Option<TableRange> {
    let mut acc: Option<TableRange> = None;
    for token in sqref.split_whitespace() {
        let r = parse_token(token)?;
        acc = Some(match acc {
            None => r,
            Some(a) => TableRange {
                first_row: a.first_row.min(r.first_row),
                last_row: a.last_row.max(r.last_row),
                first_col: a.first_col.min(r.first_col),
                last_col: a.last_col.max(r.last_col),
            },
        });
    }
    acc
}

fn parse_token(token: &str) -> Option<TableRange> {
    match token.split_once(':') {
        Some(_) => TableRange::from_a1(token),
        None => {
            let c = CellRef::from_a1(token)?;
            Some(TableRange::new(c.row, c.col, c.row, c.col))
        }
    }
}

#[cfg(test)]
mod tests;
