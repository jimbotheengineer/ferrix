//! Structured-table interoperability with Excel.
//!
//! A [`Table`] defined in Ferrix must open in Excel as a *real* Table — the
//! thing `Insert > Table` produces, with a name in the Name Box, filter
//! dropdowns on the header, banded rows, and working structured references —
//! not a range that merely looks like one. That means writing the actual OOXML
//! parts, and reading them back.
//!
//! ## Parts written
//!
//! | part / element                                | carries                     |
//! |-----------------------------------------------|-----------------------------|
//! | `xl/tables/tableN.xml`                        | name, ref, columns, style   |
//! | `<table><autoFilter>`                         | header filter dropdowns     |
//! | `<tableStyleInfo>`                            | banded rows/columns         |
//! | `<worksheet><dataValidations>`                | per-column validation       |
//! | `<worksheet><conditionalFormatting><cfRule>`  | scales, bars, thresholds    |
//! | `xl/styles.xml` `<numFmts>` + `<cols>`        | per-column number format    |
//!
//! All of it is emitted through `rust_xlsxwriter`, which owns the packaging,
//! the relationship graph and the content types. The reader goes the other
//! way and opens the `.xlsx` zip directly with `quick-xml`, because calamine
//! surfaces cell *values* and knows nothing about table parts.
//!
//! ## The Ferrix extension channel
//!
//! Three things Ferrix models have no native xlsx spelling:
//!
//! * [`ColumnType`] as distinct from a validation rule — Excel has no "this
//!   column is text" concept separate from a `dataValidation`.
//! * [`ValidationRule::Regex`] — Excel has no regex validation at all.
//! * An *active* filter predicate. Excel's table part stores filter criteria,
//!   but only in the shapes its own UI offers, and `rust_xlsxwriter` exposes
//!   no writer for a table's `filterColumn`.
//!
//! Rather than drop them, they ride in the `dataValidation` element's *input
//! message* (`promptTitle`/`prompt`) with `showInputMessage="0"`. Excel
//! preserves those attributes byte-for-byte, never displays them while the
//! flag is off, and a user editing the file in Excel is not shown anything
//! confusing. The payload is a short percent-escaped key/value string —
//! [`ferrix_tag`] / [`parse_ferrix_tag`] — and a file lacking it still imports
//! correctly, just with the extension fields at their defaults.
//!
//! Every rule that *does* have a native spelling uses it, so an Excel user
//! sees a genuine dropdown / bounds check / colour scale.
//!
//! ## What is verified, and what is not
//!
//! The tests in this module write a real `.xlsx`, unzip it, and assert on the
//! XML: that `xl/tables/table1.xml` exists with the expected `displayName` and
//! `ref`, that `dataValidations` and `conditionalFormatting` elements are
//! present with the right operators, and that re-importing reproduces every
//! rule. That proves the parts are present and well-formed, and that Ferrix
//! agrees with itself.
//!
//! It does **not** prove Excel accepts the file: Excel is not available in
//! this environment and was never launched. The claim made here is "the OOXML
//! parts Excel reads are present and structurally correct", verified by
//! inspecting the emitted XML — not "opened in Excel and confirmed".

use std::collections::HashMap;
use std::path::Path;

use ferrix_core::merge::MergeMap;
use ferrix_core::{
    CellRef, CmpOp, ColumnType, Comment, CommentMap, ConditionalRule, NumberFormat, Predicate, Rgb,
    Table, TableColumn, TableRange, Validation, ValidationRule,
};
use rust_xlsxwriter::{
    ConditionalFormat2ColorScale, ConditionalFormat3ColorScale, ConditionalFormatCell,
    ConditionalFormatCellRule, ConditionalFormatDataBar, DataValidation, DataValidationRule,
    Format, Note, Worksheet,
};

use crate::safeguard::{self, Limits, SafeguardError};
use crate::xlsx::XlsxError;

/// Read every part of an `.xlsx` under the resource safeguards.
///
/// Replaces three copies of a loop that did
/// `Vec::with_capacity(entry.size())` — sizing an allocation directly from a
/// number the file chose. A 40 KB archive claiming a 4 GB entry aborted the
/// process on the reserve, before a byte was decompressed.
fn read_package(path: &Path, limits: &Limits) -> Result<HashMap<String, Vec<u8>>, SafeguardError> {
    let disp = path.display().to_string();
    let (mut zip, _report) = safeguard::open_checked(path, limits)?;
    safeguard::read_all_parts(&mut zip, &disp, limits, None)
}

/// [`read_package`] for sibling modules that read the raw OOXML — currently
/// [`crate::protect_xlsx`]. Re-exported rather than duplicated so the
/// safeguards (declared-size checks, part budget, zip-slip) apply once.
pub(crate) fn read_package_for(
    path: &Path,
    limits: &Limits,
) -> Result<HashMap<String, Vec<u8>>, SafeguardError> {
    read_package(path, limits)
}

/// [`worksheet_paths`] for sibling modules. Same reason.
pub(crate) fn worksheet_paths_for(
    parts: &HashMap<String, Vec<u8>>,
    path: &str,
) -> Result<Vec<String>, SafeguardError> {
    worksheet_paths(parts, path)
}

/// [`attr`] for sibling modules. Same reason: entity handling and the
/// `normalized_value` workaround must not be reimplemented per module.
pub(crate) fn attr_for(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<String> {
    attr(e, name)
}

/// Sentinel bound meaning "any finite number", used to express a bare
/// [`ColumnType::Number`] as a real Excel `decimal` validation. Excel accepts
/// a rule this wide and it does exactly what the column type means: reject
/// text, accept any number.
const ANY_NUMBER_MIN: f64 = -1.0e307;
const ANY_NUMBER_MAX: f64 = 1.0e307;

/// Title of the extension-channel input message. Short by necessity — Excel
/// caps `promptTitle` at 32 characters.
pub const FERRIX_TAG_TITLE: &str = "Ferrix";

// ============================================================ extension tag ==

/// Percent-escape the characters the tag grammar reserves.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '|' => out.push_str("%7C"),
            ':' => out.push_str("%3A"),
            ',' => out.push_str("%2C"),
            _ => out.push(ch),
        }
    }
    out
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        // Not an escape: copy the whole character, not the byte, so non-ASCII
        // text survives.
        let ch = s[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Encode a column's Ferrix-only state, or `None` when there is nothing that
/// xlsx cannot already say.
pub fn ferrix_tag(col: &TableColumn) -> Option<String> {
    let mut parts: Vec<String> = vec!["fx1".into()];
    if col.ctype != ColumnType::Any {
        parts.push(format!("t={}", col.ctype.as_str()));
    }
    match &col.validation.rule {
        ValidationRule::Regex(p) => parts.push(format!("r=re:{}", esc(p))),
        ValidationRule::Unique => parts.push("r=uniq".into()),
        _ => {}
    }
    if !col.validation.allow_empty {
        parts.push("ne=1".into());
    }
    if let Some(f) = &col.filter {
        parts.push(format!("f={}", encode_predicate(f)));
    }
    (parts.len() > 1).then(|| parts.join("|"))
}

fn encode_predicate(p: &Predicate) -> String {
    match p {
        Predicate::Blank => "blank".into(),
        Predicate::NonBlank => "nonblank".into(),
        Predicate::Compare { op, value } => format!("cmp:{}:{}", op.as_xlsx(), value),
        Predicate::Between { min, max } => format!("btw:{min}:{max}"),
        Predicate::Text {
            needle,
            case_sensitive,
            whole_cell,
        } => format!(
            "txt:{}:{}:{}",
            u8::from(*case_sensitive),
            u8::from(*whole_cell),
            esc(needle)
        ),
        Predicate::ValueList(v) => {
            let joined: Vec<String> = v.iter().map(|s| esc(s)).collect();
            format!("list:{}", joined.join(","))
        }
    }
}

fn decode_predicate(s: &str) -> Option<Predicate> {
    let (kind, rest) = s.split_once(':').unwrap_or((s, ""));
    Some(match kind {
        "blank" => Predicate::Blank,
        "nonblank" => Predicate::NonBlank,
        "cmp" => {
            let (op, val) = rest.split_once(':')?;
            Predicate::Compare {
                op: CmpOp::from_xlsx(op)?,
                value: val.parse().ok()?,
            }
        }
        "btw" => {
            let (a, b) = rest.split_once(':')?;
            Predicate::Between {
                min: a.parse().ok()?,
                max: b.parse().ok()?,
            }
        }
        "txt" => {
            let mut it = rest.splitn(3, ':');
            let cs = it.next()? == "1";
            let wc = it.next()? == "1";
            Predicate::Text {
                needle: unesc(it.next()?),
                case_sensitive: cs,
                whole_cell: wc,
            }
        }
        "list" => Predicate::ValueList(
            rest.split(',')
                .filter(|s| !s.is_empty())
                .map(unesc)
                .collect(),
        ),
        _ => return None,
    })
}

/// What a parsed tag carries. Anything absent stays at its default, so a file
/// written by Excel (which has no tag at all) imports cleanly.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FerrixTag {
    pub ctype: Option<ColumnType>,
    pub rule: Option<ValidationRule>,
    pub require_value: bool,
    pub filter: Option<Predicate>,
}

/// Parse the extension payload. Returns `None` for anything that is not a
/// Ferrix tag, including an empty prompt.
pub fn parse_ferrix_tag(s: &str) -> Option<FerrixTag> {
    let mut it = s.split('|');
    if it.next()? != "fx1" {
        return None;
    }
    let mut tag = FerrixTag::default();
    for field in it {
        let (k, v) = field.split_once('=').unwrap_or((field, ""));
        match k {
            "t" => tag.ctype = ColumnType::from_str_opt(v),
            "ne" => tag.require_value = v == "1",
            "r" => {
                tag.rule = if let Some(p) = v.strip_prefix("re:") {
                    Some(ValidationRule::Regex(unesc(p)))
                } else if v == "uniq" {
                    Some(ValidationRule::Unique)
                } else {
                    None
                };
            }
            "f" => tag.filter = decode_predicate(v),
            _ => {}
        }
    }
    Some(tag)
}

// ==================================================================== write ==

/// Write a table's parts onto an already-populated worksheet.
///
/// The caller is responsible for the cell values (see
/// [`crate::xlsx::export_workbook`]); this adds the table part, the
/// validations, the conditional formats and the number formats over the same
/// range.
///
/// # Constant-memory mode
///
/// `rust_xlsxwriter`'s constant-memory worksheets flush rows as they are
/// written, which is incompatible with attaching a table afterwards. Tables
/// are therefore written onto ordinary worksheets. That is not a regression
/// for the datasets in question: a table is bounded by Excel's 1,048,576-row
/// limit regardless, and [`crate::xlsx::export_workbook`] already refuses
/// anything larger.
pub fn write_table(ws: &mut Worksheet, table: &Table) -> Result<(), rust_xlsxwriter::XlsxError> {
    let r = table.range;

    // --- number formats, per column ---
    //
    // Applied to the whole sheet column, which is what Excel's own "format
    // this table column" does. A Custom format's string goes out verbatim.
    for (i, col) in table.columns.iter().enumerate() {
        if col.format == NumberFormat::General {
            continue;
        }
        let fmt = Format::new().set_num_format(col.format.to_code());
        ws.set_column_format(table.sheet_col(i) as u16, &fmt)?;
    }

    // --- the table part itself ---
    let mut xt = rust_xlsxwriter::Table::new()
        .set_name(table.name.clone())
        .set_header_row(table.header_row)
        .set_total_row(table.totals_row)
        .set_banded_rows(table.banded_rows)
        .set_banded_columns(table.banded_cols)
        .set_autofilter(table.autofilter);
    if let Some(style) = &table.style {
        if let Some(s) = style_from_name(style) {
            xt = xt.set_style(s);
        }
    }
    let cols: Vec<rust_xlsxwriter::TableColumn> = table
        .columns
        .iter()
        .map(|c| {
            let mut tc = rust_xlsxwriter::TableColumn::new().set_header(c.name.clone());
            if let Some(f) = &c.totals_function {
                tc = tc.set_total_function(totals_function_from_name(f));
            }
            if let Some(l) = &c.totals_label {
                tc = tc.set_total_label(l.clone());
            }
            tc
        })
        .collect();
    xt = xt.set_columns(&cols);
    ws.add_table(
        r.first_row,
        r.first_col as u16,
        r.last_row,
        r.last_col as u16,
        &xt,
    )?;

    // --- per-column validation and conditional formats over the data rows ---
    let rows = table.data_rows();
    if rows.is_empty() {
        return Ok(());
    }
    let (r0, r1) = (rows.start, rows.end - 1);

    for (i, col) in table.columns.iter().enumerate() {
        let c = table.sheet_col(i) as u16;

        if let Some(dv) = build_validation(col)? {
            ws.add_data_validation(r0, c, r1, c, &dv)?;
        }

        for rule in &col.conditional {
            write_conditional(ws, r0, c, r1, rule)?;
        }
    }
    Ok(())
}

/// Build the `dataValidation` for a column, or `None` when the column has
/// nothing to say.
fn build_validation(
    col: &TableColumn,
) -> Result<Option<DataValidation>, rust_xlsxwriter::XlsxError> {
    let tag = ferrix_tag(col);
    let has_native = !matches!(
        col.validation.rule,
        ValidationRule::None | ValidationRule::Regex(_) | ValidationRule::Unique
    ) || col.ctype != ColumnType::Any;
    if tag.is_none() && !has_native {
        return Ok(None);
    }

    let mut dv = DataValidation::new().ignore_blank(col.validation.allow_empty);

    dv = match (&col.validation.rule, col.ctype) {
        (ValidationRule::Between { min, max }, ColumnType::Date) => {
            // Serial dates are numbers to Ferrix; `decimal` keeps the bound
            // values exact, where `date` would demand a datetime literal.
            dv.allow_decimal_number(DataValidationRule::Between(*min, *max))
        }
        (ValidationRule::Between { min, max }, _) => {
            dv.allow_decimal_number(DataValidationRule::Between(*min, *max))
        }
        (ValidationRule::NotBetween { min, max }, _) => {
            dv.allow_decimal_number(DataValidationRule::NotBetween(*min, *max))
        }
        (ValidationRule::Compare { op, value }, _) => {
            dv.allow_decimal_number(cmp_rule(*op, *value))
        }
        (ValidationRule::OneOf(list), _) => {
            let refs: Vec<&str> = list.iter().map(String::as_str).collect();
            dv.allow_list_strings(&refs)?
        }
        (ValidationRule::TextLength { min, max }, _) => {
            dv.allow_text_length(DataValidationRule::Between(*min, *max))
        }
        // Regex and Unique have no Excel spelling. They are carried in the
        // extension tag; the native rule is the column's type check so Excel
        // still enforces something meaningful rather than nothing.
        (_, ColumnType::Number) | (_, ColumnType::Date) => {
            dv.allow_decimal_number(DataValidationRule::Between(ANY_NUMBER_MIN, ANY_NUMBER_MAX))
        }
        (_, ColumnType::Text) => dv.allow_text_length(DataValidationRule::GreaterThanOrEqualTo(0)),
        (_, ColumnType::Bool) => dv.allow_list_strings(&["TRUE", "FALSE"])?,
        (_, ColumnType::Any) => dv.allow_any_value(),
    };

    if let Some(msg) = &col.validation.message {
        dv = dv.set_error_message(truncate(msg, 255))?;
    }
    if let Some(tag) = tag {
        // The extension channel. `show_input_message(false)` is what keeps it
        // invisible in Excel while still being written to the file.
        dv = dv
            .set_input_title(FERRIX_TAG_TITLE)?
            .set_input_message(truncate(&tag, 255))?
            .show_input_message(false);
    }
    Ok(Some(dv))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

fn cmp_rule(op: CmpOp, v: f64) -> DataValidationRule<f64> {
    cmp_rule_generic(op, v)
}

/// [`cmp_rule`] over any value type the writer accepts.
///
/// Generic because sheet-range validation (issue #41) needs the SAME operator
/// mapping for `i32` whole numbers and `u32` text lengths. One function, so a
/// `>=` cannot mean one thing for a table column and another for a range.
pub(crate) fn cmp_rule_generic<T: rust_xlsxwriter::IntoDataValidationValue>(
    op: CmpOp,
    v: T,
) -> DataValidationRule<T> {
    match op {
        CmpOp::Eq => DataValidationRule::EqualTo(v),
        CmpOp::Ne => DataValidationRule::NotEqualTo(v),
        CmpOp::Lt => DataValidationRule::LessThan(v),
        CmpOp::Le => DataValidationRule::LessThanOrEqualTo(v),
        CmpOp::Gt => DataValidationRule::GreaterThan(v),
        CmpOp::Ge => DataValidationRule::GreaterThanOrEqualTo(v),
    }
}

pub(crate) fn to_color(c: Rgb) -> rust_xlsxwriter::Color {
    rust_xlsxwriter::Color::RGB(c.to_u32())
}

fn write_conditional(
    ws: &mut Worksheet,
    r0: u32,
    c: u16,
    r1: u32,
    rule: &ConditionalRule,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    match rule {
        ConditionalRule::ColorScale2 { min, max } => {
            let cf = ConditionalFormat2ColorScale::new()
                .set_minimum_color(to_color(*min))
                .set_maximum_color(to_color(*max));
            ws.add_conditional_format(r0, c, r1, c, &cf)?;
        }
        ConditionalRule::ColorScale3 { min, mid, max } => {
            let cf = ConditionalFormat3ColorScale::new()
                .set_minimum_color(to_color(*min))
                .set_midpoint_color(to_color(*mid))
                .set_maximum_color(to_color(*max));
            ws.add_conditional_format(r0, c, r1, c, &cf)?;
        }
        ConditionalRule::DataBar { color } => {
            let cf = ConditionalFormatDataBar::new().set_fill_color(to_color(*color));
            ws.add_conditional_format(r0, c, r1, c, &cf)?;
        }
        ConditionalRule::Threshold {
            op,
            value,
            fill,
            text,
        } => {
            let fmt = Format::new()
                .set_background_color(to_color(*fill))
                .set_font_color(to_color(*text));
            let cf = ConditionalFormatCell::new()
                .set_rule(cell_rule(*op, *value))
                .set_format(fmt);
            ws.add_conditional_format(r0, c, r1, c, &cf)?;
        }

        // Sign colouring is two Excel cell rules: < 0 and > 0. Excel has no
        // single "colour by sign" construct, so expressing it as the pair it
        // already is keeps the meaning intact across the round trip rather
        // than dropping it.
        ConditionalRule::Sign {
            negative,
            positive,
            zero,
        } => {
            if let Some(neg) = negative {
                let cf = ConditionalFormatCell::new()
                    .set_rule(ConditionalFormatCellRule::LessThan(0.0))
                    .set_format(Format::new().set_background_color(to_color(*neg)));
                ws.add_conditional_format(r0, c, r1, c, &cf)?;
            }
            if let Some(pos) = positive {
                let cf = ConditionalFormatCell::new()
                    .set_rule(ConditionalFormatCellRule::GreaterThan(0.0))
                    .set_format(Format::new().set_background_color(to_color(*pos)));
                ws.add_conditional_format(r0, c, r1, c, &cf)?;
            }
            if let Some(z) = zero {
                let cf = ConditionalFormatCell::new()
                    .set_rule(ConditionalFormatCellRule::EqualTo(0.0))
                    .set_format(Format::new().set_background_color(to_color(*z)));
                ws.add_conditional_format(r0, c, r1, c, &cf)?;
            }
        }

        // A manual colour has no condition at all. Excel's nearest equivalent
        // is a rule that always holds; "not equal to a sentinel no cell will
        // hold" is the conventional spelling, and it survives round-tripping
        // where a bare cell format on a table column does not.
        ConditionalRule::Manual {
            fill,
            text,
            typography,
        } => {
            let mut fmt = Format::new();
            if let Some(f) = fill {
                fmt = fmt.set_background_color(to_color(*f));
            }
            if let Some(t) = text {
                fmt = fmt.set_font_color(to_color(*t));
            }
            // Type styling maps onto real OOXML font attributes, so bold and
            // friends survive the trip into Excel rather than being dropped.
            if typography.bold == Some(true) {
                fmt = fmt.set_bold();
            }
            if typography.italic == Some(true) {
                fmt = fmt.set_italic();
            }
            if typography.underline == Some(true) {
                fmt = fmt.set_underline(rust_xlsxwriter::FormatUnderline::Single);
            }
            if let Some(pt) = typography.size {
                fmt = fmt.set_font_size(pt);
            }
            if let Some(fam) = typography.family {
                fmt = fmt.set_font_name(match fam {
                    ferrix_core::format::FontFamily::Monospace => "Consolas",
                    ferrix_core::format::FontFamily::Proportional => "Calibri",
                });
            }
            let cf = ConditionalFormatCell::new()
                .set_rule(ConditionalFormatCellRule::NotEqualTo(f64::MIN))
                .set_format(fmt);
            ws.add_conditional_format(r0, c, r1, c, &cf)?;
        }

        // Top/bottom N and text-contains have no lossless mapping through the
        // conditional-format types this crate exposes. Skipping them SILENTLY
        // would be the worst option -- the user would see a rule in Ferrix and
        // not in Excel with no explanation -- so they are deliberately dropped
        // here and reported by the caller instead. See
        // `unsupported_rules_are_reported`.
        ConditionalRule::TopBottom { .. } | ConditionalRule::TextContains { .. } => {}
    }
    Ok(())
}

/// True when a rule cannot be represented in xlsx and will be dropped on
/// export, so the caller can tell the user which rules did not survive rather
/// than letting them discover it in Excel.
pub fn rule_survives_xlsx(rule: &ConditionalRule) -> bool {
    !matches!(
        rule,
        ConditionalRule::TopBottom { .. } | ConditionalRule::TextContains { .. }
    )
}

fn cell_rule(op: CmpOp, v: f64) -> ConditionalFormatCellRule<f64> {
    match op {
        CmpOp::Eq => ConditionalFormatCellRule::EqualTo(v),
        CmpOp::Ne => ConditionalFormatCellRule::NotEqualTo(v),
        CmpOp::Lt => ConditionalFormatCellRule::LessThan(v),
        CmpOp::Le => ConditionalFormatCellRule::LessThanOrEqualTo(v),
        CmpOp::Gt => ConditionalFormatCellRule::GreaterThan(v),
        CmpOp::Ge => ConditionalFormatCellRule::GreaterThanOrEqualTo(v),
    }
}

/// Excel's built-in table style names. An unrecognised name is dropped from
/// the emitted part but kept on the Ferrix side, so nothing is silently
/// rewritten to a different style.
fn style_from_name(name: &str) -> Option<rust_xlsxwriter::TableStyle> {
    use rust_xlsxwriter::TableStyle as S;
    Some(match name {
        "None" => S::None,
        "TableStyleLight1" => S::Light1,
        "TableStyleLight9" => S::Light9,
        "TableStyleMedium2" => S::Medium2,
        "TableStyleMedium9" => S::Medium9,
        "TableStyleDark1" => S::Dark1,
        _ => return None,
    })
}

fn totals_function_from_name(name: &str) -> rust_xlsxwriter::TableFunction {
    use rust_xlsxwriter::TableFunction as F;
    match name.to_ascii_lowercase().as_str() {
        "average" => F::Average,
        "count" => F::Count,
        "countnums" => F::CountNumbers,
        "max" => F::Max,
        "min" => F::Min,
        "stddev" => F::StdDev,
        "sum" => F::Sum,
        "var" => F::Var,
        _ => F::None,
    }
}

// ===================================================================== read ==

/// A merged region found in a worksheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedMerge {
    /// Index of the owning worksheet, in workbook order.
    pub sheet_index: usize,
    pub range: TableRange,
}

/// Read every `<mergeCell>` out of an `.xlsx`.
///
/// Ferrix previously dropped these entirely. A merged title row then came back
/// as a value in the first column with blanks beside it, which reads as data
/// loss to the user even though every byte survived.
///
/// Opens the package directly for the same reason table import does: calamine
/// surfaces cell values, not the worksheet's structural parts.
pub fn import_merges(path: impl AsRef<Path>) -> Result<Vec<ImportedMerge>, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();
    let parts = read_package(path, &Limits::measured())?;

    let sheet_paths = worksheet_paths(&parts, &disp)?;
    let mut out = Vec::new();
    for (sheet_index, sp) in sheet_paths.iter().enumerate() {
        let Some(xml) = parts.get(sp) else { continue };
        safeguard::scan_part(xml, &disp, sp, None, |ev| {
            use quick_xml::events::Event as E;
            if let E::Empty(e) | E::Start(e) = ev {
                if e.local_name().as_ref() == b"mergeCell" {
                    if let Some(r) = attr(e, b"ref").and_then(|r| TableRange::from_a1(&r)) {
                        out.push(ImportedMerge {
                            sheet_index,
                            range: r,
                        });
                    }
                }
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// Write `<mergeCells>` for a sheet's merged regions.
///
/// Excel requires the count attribute to match the number of children; an
/// inconsistent pair makes the whole workbook unopenable, so it is derived
/// here rather than passed in.
pub fn write_merges(
    ws: &mut Worksheet,
    merges: &MergeMap,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    for r in merges.regions() {
        // rust_xlsxwriter writes the merge and its anchor format together; a
        // default format keeps the cell's existing appearance.
        ws.merge_range(
            r.first_row,
            r.first_col as u16,
            r.last_row,
            r.last_col as u16,
            "",
            &Format::new(),
        )?;
    }
    Ok(())
}

/// A cell comment found in a worksheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedComment {
    /// Index of the owning worksheet, in workbook order.
    pub sheet_index: usize,
    pub cell: CellRef,
    pub comment: Comment,
}

/// Default author written for a comment the user left unattributed.
///
/// `xl/comments1.xml` addresses every comment through an `authorId` into an
/// `<authors>` list, so there is no spelling for "no author". Rather than
/// invent a blank entry Excel would render as a stray colon, unattributed
/// comments are written under this name and mapped back to an empty author on
/// import — so an unattributed comment stays unattributed across the trip.
pub const DEFAULT_COMMENT_AUTHOR: &str = "Ferrix";

/// Write a sheet's comments as real Excel notes.
///
/// ## What Excel shows, and what it does not
///
/// `rust_xlsxwriter`'s `insert_note` emits the whole legacy note apparatus,
/// not just the text part: `xl/comments1.xml` carries the author list and the
/// comment bodies, and the VML drawing (`xl/drawings/vmlDrawing1.vml`, plus
/// its rels and the `<legacyDrawing>` element on the worksheet) carries the
/// yellow box's geometry. Both are required — a comments part on its own is
/// silently ignored by Excel, which is the failure mode the task warned about,
/// and it is avoided here by writing through the library that owns both parts
/// rather than hand-rolling the XML.
///
/// So an exported comment appears in Excel as an ordinary note: red marker
/// triangle in the cell's corner, text on hover, author shown in the box.
///
/// What is deliberately NOT written, matching v1's scope:
///
/// * **No reply threading.** Modern Excel's threaded comments are a separate
///   pair of parts (`xl/threadedComments/*.xml` plus a persons list) and a
///   different data model — a chain of authored replies rather than one note.
///   Ferrix stores one author and one body per cell, so it writes the legacy
///   note, which every Excel version since 97 reads. Excel will offer to
///   "convert to a threaded comment"; nothing is lost if the user accepts.
/// * **No box geometry, colour, or visibility.** The note uses the library's
///   defaults (hidden until hover, standard size). Ferrix has no model for
///   any of it, so there is nothing to round-trip.
///
/// The tests unzip the written package and assert `xl/comments1.xml` and the
/// VML part are both present with the expected text — but Excel itself is not
/// available in this environment and was never launched, so the claim is "the
/// parts Excel reads are present and structurally correct", not "opened in
/// Excel and confirmed".
pub fn write_comments(
    ws: &mut Worksheet,
    comments: &CommentMap,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    for (cell, c) in comments.iter() {
        // Excel's own column ceiling; a comment past it would make the whole
        // package unopenable, and dropping one note is better than that.
        let Ok(col) = u16::try_from(cell.col) else {
            continue;
        };
        let author = if c.author.is_empty() {
            DEFAULT_COMMENT_AUTHOR
        } else {
            c.author.as_str()
        };
        let note = Note::new(&c.text)
            .set_author(author)
            // The author prefix would bake "ana:" into the TEXT, so a
            // round-trip would grow the prefix again on every save. The author
            // travels in its own attribute instead.
            .add_author_prefix(false);
        ws.insert_note(cell.row, col, &note)?;
    }
    Ok(())
}

/// Read every cell comment out of an `.xlsx`.
///
/// Opens the package directly, like [`import_tables`]: calamine surfaces cell
/// values and knows nothing about `xl/comments*.xml`.
///
/// A workbook with no comments returns an empty vector; that is not an error.
pub fn import_comments(path: impl AsRef<Path>) -> Result<Vec<ImportedComment>, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();
    let parts = read_package(path, &Limits::measured())?;

    let mut out = Vec::new();
    for (sheet_index, sp) in worksheet_paths(&parts, &disp)?.iter().enumerate() {
        // The comments part is reached through the WORKSHEET's relationships,
        // not by guessing `comments{n}.xml` matches sheet n. Excel numbers the
        // parts by the order sheets that *have* comments appear, so sheet 3
        // may own comments1.xml.
        let rels = rels_for(&parts, sp, &disp)?;
        let Some(target) = rels
            .values()
            .find(|t| t.contains("comments") && t.ends_with(".xml"))
        else {
            continue;
        };
        let Some(xml) = parts.get(target) else {
            continue;
        };
        for (cell, comment) in parse_comments_part(xml, &disp, target)? {
            out.push(ImportedComment {
                sheet_index,
                cell,
                comment,
            });
        }
    }
    Ok(out)
}

/// Parse one `xl/comments*.xml` into (cell, comment) pairs.
///
/// The text of a comment is a rich-text run sequence — `<text><r><t>..</t></r>
/// ...</text>` — so every `<t>` inside one `<comment>` is concatenated. Taking
/// only the first would silently truncate any note Excel split into runs, and
/// Excel splits on the smallest formatting change.
fn parse_comments_part(
    xml: &[u8],
    path: &str,
    part: &str,
) -> Result<Vec<(CellRef, Comment)>, SafeguardError> {
    let mut authors: Vec<String> = Vec::new();
    let mut out = Vec::new();

    let mut in_authors = false;
    let mut in_author = false;
    let mut current: Option<(CellRef, usize)> = None;
    let mut in_t = false;
    let mut text = String::new();
    let mut author_text = String::new();

    safeguard::scan_part(xml, path, part, None, |ev| {
        use quick_xml::events::Event as E;
        match ev {
            E::Start(e) => match e.local_name().as_ref() {
                b"authors" => in_authors = true,
                b"author" if in_authors => {
                    in_author = true;
                    author_text.clear();
                }
                b"comment" => {
                    let cell = attr(e, b"ref").and_then(|r| CellRef::from_a1(&r));
                    let id = attr(e, b"authorId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    current = cell.map(|c| (c, id));
                    text.clear();
                }
                b"t" => in_t = true,
                _ => {}
            },
            E::Text(t) => {
                let s = t.decode().map(|c| c.into_owned()).unwrap_or_default();
                if in_author {
                    author_text.push_str(&s);
                } else if in_t {
                    text.push_str(&s);
                }
            }
            // quick-xml 0.41 reports entity references as their own event
            // rather than folding them into the surrounding Text. Ignoring
            // them would silently DELETE every `<`, `&` and `>` a user typed
            // into a note, which is exactly the kind of quiet corruption an
            // annotation must never suffer.
            //
            // `scan_part` has already refused anything that is not one of the
            // five predefined entities or a numeric character reference, so
            // by the time this arm runs the reference is known-safe.
            E::GeneralRef(r) => {
                let Some(dst) = (if in_author {
                    Some(&mut author_text)
                } else if in_t {
                    Some(&mut text)
                } else {
                    None
                }) else {
                    return Ok(());
                };
                if let Ok(Some(ch)) = r.resolve_char_ref() {
                    dst.push(ch);
                } else {
                    match r.decode().as_deref() {
                        Ok("amp") => dst.push('&'),
                        Ok("lt") => dst.push('<'),
                        Ok("gt") => dst.push('>'),
                        Ok("quot") => dst.push('"'),
                        Ok("apos") => dst.push('\''),
                        Ok(_) | Err(_) => {}
                    }
                }
            }
            E::End(e) => match e.local_name().as_ref() {
                b"authors" => in_authors = false,
                b"author" if in_author => {
                    in_author = false;
                    authors.push(std::mem::take(&mut author_text));
                }
                b"t" => in_t = false,
                b"comment" => {
                    if let Some((cell, id)) = current.take() {
                        let author = authors.get(id).cloned().unwrap_or_default();
                        // The placeholder written for an unattributed comment
                        // maps back to empty, so a note the user never signed
                        // does not acquire an author by making a round trip.
                        let author = if author == DEFAULT_COMMENT_AUTHOR {
                            String::new()
                        } else {
                            author
                        };
                        out.push((
                            cell,
                            Comment {
                                author,
                                text: std::mem::take(&mut text),
                            },
                        ));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    })?;
    Ok(out)
}

/// Every table found in a workbook, with the worksheet it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedTable {
    /// Index of the owning worksheet, in workbook order.
    pub sheet_index: usize,
    pub table: Table,
}

/// Read every structured table out of an `.xlsx`.
///
/// Opens the package directly rather than going through calamine, because the
/// parts we need — `xl/tables/*.xml`, `dataValidations`,
/// `conditionalFormatting`, `numFmts` — are not cell data and calamine does
/// not surface them.
///
/// A workbook with no tables returns an empty vector; that is not an error.
pub fn import_tables(path: impl AsRef<Path>) -> Result<Vec<ImportedTable>, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();

    // Read the whole package into memory up front, under the safeguards. An
    // xlsx is bounded by Excel's own limits and this avoids fighting the
    // archive's borrow of itself while chasing relationships across parts.
    let parts = read_package(path, &Limits::measured())?;

    let styles = parse_styles(
        parts.get("xl/styles.xml").map(Vec::as_slice),
        &disp,
        "xl/styles.xml",
    )?;
    let sheet_paths = worksheet_paths(&parts, &disp)?;

    let mut out = Vec::new();
    for (sheet_index, sheet_path) in sheet_paths.iter().enumerate() {
        let Some(xml) = parts.get(sheet_path) else {
            continue;
        };
        let sheet = parse_worksheet(xml, &styles, &disp, sheet_path)?;
        if sheet.table_rels.is_empty() {
            continue;
        }
        let rels = rels_for(&parts, sheet_path, &disp)?;
        for rid in &sheet.table_rels {
            let Some(target) = rels.get(rid) else {
                continue;
            };
            let Some(table_xml) = parts.get(target) else {
                continue;
            };
            let mut table = parse_table_part(table_xml, &disp, target)?;
            apply_sheet_decorations(&mut table, &sheet);
            out.push(ImportedTable { sheet_index, table });
        }
    }
    Ok(out)
}

/// Worksheet part paths in workbook order.
///
/// Follows `xl/workbook.xml` -> `xl/_rels/workbook.xml.rels` rather than
/// guessing `sheet1.xml, sheet2.xml, ...`: the numbering is a convention, not
/// a guarantee, and a workbook that has had sheets deleted breaks it.
///
/// A malformed `xl/workbook.xml` is an ERROR here, not a short list. The
/// index this returns is what every `sheet_index` in the results refers to,
/// so silently returning three sheets for a five-sheet workbook would
/// misattribute every merge, comment and table in the last two.
fn worksheet_paths(
    parts: &HashMap<String, Vec<u8>>,
    path: &str,
) -> Result<Vec<String>, SafeguardError> {
    const PART: &str = "xl/workbook.xml";
    let Some(xml) = parts.get(PART) else {
        return Ok(Vec::new());
    };
    let rels = rels_for(parts, PART, path)?;
    let mut ids = Vec::new();
    safeguard::scan_part(xml, path, PART, None, |ev| {
        use quick_xml::events::Event as E;
        if let E::Empty(e) | E::Start(e) = ev {
            if e.local_name().as_ref() == b"sheet" {
                if let Some(id) = attr(e, b"id").or_else(|| attr(e, b"r:id")) {
                    ids.push(id);
                }
            }
        }
        Ok(())
    })?;
    Ok(ids
        .iter()
        .filter_map(|id| rels.get(id).cloned())
        .filter(|p| p.contains("worksheets/"))
        .collect())
}

/// Resolve a part's `_rels` file into id -> absolute part path.
fn rels_for(
    parts: &HashMap<String, Vec<u8>>,
    part: &str,
    path: &str,
) -> Result<HashMap<String, String>, SafeguardError> {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let rel_path = if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}/_rels/{file}.rels")
    };
    let mut map = HashMap::new();
    let Some(xml) = parts.get(&rel_path) else {
        return Ok(map);
    };
    safeguard::scan_part(xml, path, &rel_path, None, |ev| {
        use quick_xml::events::Event as E;
        if let E::Empty(e) | E::Start(e) = ev {
            if e.local_name().as_ref() == b"Relationship" {
                if let (Some(id), Some(target)) = (attr(e, b"Id"), attr(e, b"Target")) {
                    map.insert(id, normalise_target(dir, &target));
                }
            }
        }
        Ok(())
    })?;
    Ok(map)
}

/// Turn a relationship target into a package-absolute path, resolving the
/// `../` a worksheet uses to reach `xl/tables/`.
fn normalise_target(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut segs: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    segs.join("/")
}

/// Read an attribute by name, resolving XML entities.
///
/// The unescaping is load-bearing: a currency format code is written as
/// `formatCode="&quot;$&quot;#,##0.00"`, and comparing the raw attribute
/// bytes would make every quoted format look like an unknown custom one.
fn attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        let key = a.key.as_ref();
        let local = a.key.local_name();
        if key != name && local.as_ref() != name {
            return None;
        }
        // `normalized_value` rather than the deprecated `unescape_value`: the
        // latter is compiled out when quick-xml's `encoding` feature is on,
        // and calamine turns it on for the whole build via feature unification.
        Some(
            match a.normalized_value(quick_xml::XmlVersion::Implicit1_0) {
                Ok(v) => v.into_owned(),
                // A malformed entity is not worth failing an import over; the
                // raw text is closer to the truth than dropping the attribute.
                Err(_) => String::from_utf8_lossy(a.value.as_ref()).into_owned(),
            },
        )
    })
}

/// numFmtId -> format code, plus the cellXfs and dxfs a sheet refers to.
#[derive(Debug, Default)]
struct Styles {
    /// Style index (`s` attribute / `<col style>`) -> format code.
    xf_formats: Vec<String>,
    /// dxf index -> (fill, font colour), for `cellIs` conditional rules.
    dxfs: Vec<(Option<Rgb>, Option<Rgb>)>,
}

/// The subset of Excel's built-in number formats Ferrix needs to recognise by
/// id. Anything not listed keeps its literal `formatCode` from `<numFmts>`.
fn builtin_format(id: u32) -> Option<&'static str> {
    Some(match id {
        0 => "General",
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        14 => "mm/dd/yyyy",
        22 => "yyyy-mm-dd hh:mm:ss",
        _ => return None,
    })
}

fn parse_styles(xml: Option<&[u8]>, path: &str, part: &str) -> Result<Styles, SafeguardError> {
    let mut styles = Styles::default();
    let Some(xml) = xml else { return Ok(styles) };
    let mut num_fmts: HashMap<u32, String> = HashMap::new();
    let mut in_cell_xfs = false;
    let mut in_dxfs = false;
    let mut dxf_depth = 0usize;
    let mut cur_dxf: (Option<Rgb>, Option<Rgb>) = (None, None);
    let mut xf_ids: Vec<u32> = Vec::new();

    // A malformed styles part is an ERROR, not a stopping point. Truncating
    // `<cellXfs>` shifts every later style index, so the previous "treat it
    // like EOF" behaviour rendered columns with the WRONG number format
    // rather than reporting that the file was damaged.
    safeguard::scan_part(xml, path, part, None, |ev| {
        use quick_xml::events::Event as E;
        match ev {
            E::Start(e) | E::Empty(e) => {
                let empty = matches!(ev, E::Empty(_));
                match e.local_name().as_ref() {
                    b"numFmt" => {
                        if let (Some(id), Some(code)) =
                            (attr(e, b"numFmtId"), attr(e, b"formatCode"))
                        {
                            if let Ok(id) = id.parse::<u32>() {
                                num_fmts.insert(id, code);
                            }
                        }
                    }
                    b"cellXfs" => in_cell_xfs = true,
                    b"dxfs" => in_dxfs = true,
                    b"dxf" if in_dxfs => {
                        dxf_depth += 1;
                        cur_dxf = (None, None);
                        if empty {
                            styles.dxfs.push(cur_dxf);
                            dxf_depth -= 1;
                        }
                    }
                    b"xf" if in_cell_xfs => {
                        xf_ids.push(
                            attr(e, b"numFmtId")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0),
                        );
                    }
                    b"fgColor" if dxf_depth > 0 => {
                        cur_dxf.0 = attr(e, b"rgb").and_then(|s| Rgb::from_hex(&s));
                    }
                    b"color" if dxf_depth > 0 && cur_dxf.1.is_none() => {
                        cur_dxf.1 = attr(e, b"rgb").and_then(|s| Rgb::from_hex(&s));
                    }
                    _ => {}
                }
            }
            E::End(e) => match e.local_name().as_ref() {
                b"cellXfs" => in_cell_xfs = false,
                b"dxfs" => in_dxfs = false,
                b"dxf" if dxf_depth > 0 => {
                    styles.dxfs.push(cur_dxf);
                    dxf_depth -= 1;
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    })?;

    styles.xf_formats = xf_ids
        .into_iter()
        .map(|id| {
            num_fmts
                .get(&id)
                .cloned()
                .or_else(|| builtin_format(id).map(str::to_string))
                .unwrap_or_else(|| "General".to_string())
        })
        .collect();
    Ok(styles)
}

/// Everything a worksheet part contributes to a table's definition.
#[derive(Debug, Default)]
struct SheetDecorations {
    table_rels: Vec<String>,
    /// Column index -> number format code.
    col_formats: HashMap<u32, String>,
    /// Column index -> parsed validation.
    validations: HashMap<u32, ParsedValidation>,
    /// Column index -> conditional rules, in document order.
    conditionals: HashMap<u32, Vec<ConditionalRule>>,
}

#[derive(Debug, Default, Clone)]
struct ParsedValidation {
    rule: ValidationRule,
    ctype: ColumnType,
    allow_blank: bool,
    message: Option<String>,
    tag: Option<FerrixTag>,
}

fn parse_worksheet(
    xml: &[u8],
    styles: &Styles,
    path: &str,
    part: &str,
) -> Result<SheetDecorations, SafeguardError> {
    let mut out = SheetDecorations::default();

    // dataValidation state
    let mut dv: Option<PartialDv> = None;
    // conditionalFormatting state
    let mut cf_cols: Vec<u32> = Vec::new();
    let mut cf: Option<PartialCf> = None;
    let mut text_target: Option<TextTarget> = None;

    // A malformed worksheet part is an ERROR, not a stopping point. The
    // previous `while let Ok(..)` treated a truncated sheet exactly like a
    // complete one and returned whatever decorations happened to be parsed
    // first — a table silently losing its validations and conditional rules.
    safeguard::scan_part(xml, path, part, None, |ev| {
        use quick_xml::events::Event as E;
        match ev {
            E::Start(e) | E::Empty(e) => {
                let is_empty = matches!(ev, E::Empty(_));
                match e.local_name().as_ref() {
                    b"tablePart" => {
                        if let Some(id) = attr(e, b"id") {
                            out.table_rels.push(id);
                        }
                    }
                    b"col" => {
                        let style = attr(e, b"style").and_then(|s| s.parse::<usize>().ok());
                        let code = style
                            .and_then(|i| styles.xf_formats.get(i))
                            .cloned()
                            .unwrap_or_default();
                        if !code.is_empty() && code != "General" {
                            let lo: u32 = attr(e, b"min").and_then(|s| s.parse().ok()).unwrap_or(1);
                            let hi: u32 =
                                attr(e, b"max").and_then(|s| s.parse().ok()).unwrap_or(lo);
                            // xlsx column numbers are 1-based.
                            for c in lo..=hi.min(lo + 1024) {
                                out.col_formats.insert(c - 1, code.clone());
                            }
                        }
                    }
                    b"dataValidation" => {
                        let mut p = PartialDv {
                            kind: attr(e, b"type").unwrap_or_else(|| "any".into()),
                            operator: attr(e, b"operator"),
                            allow_blank: attr(e, b"allowBlank").as_deref() != Some("0"),
                            cols: cols_of_sqref(&attr(e, b"sqref").unwrap_or_default()),
                            prompt: attr(e, b"prompt"),
                            error: attr(e, b"error"),
                            f1: None,
                            f2: None,
                        };
                        // Excel sometimes writes formula1 as an attribute.
                        p.f1 = attr(e, b"formula1");
                        p.f2 = attr(e, b"formula2");
                        if is_empty {
                            finish_dv(p, &mut out);
                        } else {
                            dv = Some(p);
                        }
                    }
                    b"formula1" if dv.is_some() => text_target = Some(TextTarget::Dv1),
                    b"formula2" if dv.is_some() => text_target = Some(TextTarget::Dv2),
                    b"conditionalFormatting" => {
                        cf_cols = cols_of_sqref(&attr(e, b"sqref").unwrap_or_default());
                    }
                    b"cfRule" => {
                        cf = Some(PartialCf {
                            kind: attr(e, b"type").unwrap_or_default(),
                            operator: attr(e, b"operator"),
                            dxf: attr(e, b"dxfId").and_then(|s| s.parse::<usize>().ok()),
                            colors: Vec::new(),
                            formula: None,
                        });
                        if is_empty {
                            if let Some(c) = cf.take() {
                                finish_cf(c, &cf_cols, styles, &mut out);
                            }
                        }
                    }
                    b"formula" if cf.is_some() => text_target = Some(TextTarget::Cf),
                    b"color" => {
                        if let Some(c) = cf.as_mut() {
                            if let Some(rgb) = attr(e, b"rgb").and_then(|s| Rgb::from_hex(&s)) {
                                c.colors.push(rgb);
                            }
                        }
                    }
                    _ => {}
                }
            }
            E::Text(t) => {
                let s = String::from_utf8_lossy(t.as_ref()).into_owned();
                match text_target {
                    Some(TextTarget::Dv1) => {
                        if let Some(d) = dv.as_mut() {
                            d.f1 = Some(s);
                        }
                    }
                    Some(TextTarget::Dv2) => {
                        if let Some(d) = dv.as_mut() {
                            d.f2 = Some(s);
                        }
                    }
                    Some(TextTarget::Cf) => {
                        if let Some(c) = cf.as_mut() {
                            c.formula = Some(s);
                        }
                    }
                    None => {}
                }
            }
            E::End(e) => match e.local_name().as_ref() {
                b"formula1" | b"formula2" | b"formula" => text_target = None,
                b"dataValidation" => {
                    if let Some(p) = dv.take() {
                        finish_dv(p, &mut out);
                    }
                }
                b"cfRule" => {
                    if let Some(c) = cf.take() {
                        finish_cf(c, &cf_cols, styles, &mut out);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    })?;
    Ok(out)
}

enum TextTarget {
    Dv1,
    Dv2,
    Cf,
}

struct PartialDv {
    kind: String,
    operator: Option<String>,
    allow_blank: bool,
    cols: Vec<u32>,
    prompt: Option<String>,
    error: Option<String>,
    f1: Option<String>,
    f2: Option<String>,
}

struct PartialCf {
    kind: String,
    operator: Option<String>,
    dxf: Option<usize>,
    colors: Vec<Rgb>,
    formula: Option<String>,
}

/// Column indices covered by an `sqref` like `"B2:B100 D2:D100"`.
fn cols_of_sqref(sqref: &str) -> Vec<u32> {
    let mut cols = Vec::new();
    for part in sqref.split_whitespace() {
        let (a, b) = part.split_once(':').unwrap_or((part, part));
        let (Some(a), Some(b)) = (
            ferrix_core::CellRef::from_a1(a),
            ferrix_core::CellRef::from_a1(b),
        ) else {
            continue;
        };
        for c in a.col.min(b.col)..=a.col.max(b.col) {
            if !cols.contains(&c) {
                cols.push(c);
            }
        }
    }
    cols
}

fn num(s: &Option<String>) -> Option<f64> {
    s.as_ref()?.trim().trim_matches('"').parse().ok()
}

fn finish_dv(p: PartialDv, out: &mut SheetDecorations) {
    let tag = p.prompt.as_deref().and_then(parse_ferrix_tag);

    let (rule, ctype) = match p.kind.as_str() {
        "list" => {
            // Excel spells an inline list as a quoted comma-separated literal.
            let raw = p.f1.clone().unwrap_or_default();
            let items: Vec<String> = raw
                .trim()
                .trim_matches('"')
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let is_bool_shim = items.len() == 2
                && items.iter().any(|s| s.eq_ignore_ascii_case("true"))
                && items.iter().any(|s| s.eq_ignore_ascii_case("false"));
            if is_bool_shim {
                (ValidationRule::None, ColumnType::Bool)
            } else {
                (ValidationRule::OneOf(items), ColumnType::Text)
            }
        }
        "textLength" => {
            let min = num(&p.f1).unwrap_or(0.0) as u32;
            let max = num(&p.f2).unwrap_or(f64::from(u32::MAX)) as u32;
            // Excel omits `operator` when it is "between" (the default), so a
            // second formula is the reliable signal for a two-sided rule.
            let two_sided = p.operator.as_deref() == Some("between")
                || (p.operator.is_none() && p.f2.is_some());
            if two_sided {
                (ValidationRule::TextLength { min, max }, ColumnType::Text)
            } else {
                // The "any text" shim written for a bare Text column.
                (ValidationRule::None, ColumnType::Text)
            }
        }
        "decimal" | "whole" | "date" => {
            let a = num(&p.f1).unwrap_or(0.0);
            let b = num(&p.f2).unwrap_or(0.0);
            let ctype = if p.kind == "date" {
                ColumnType::Date
            } else {
                ColumnType::Number
            };
            match p.operator.as_deref() {
                Some("between") | None => {
                    // The "any number" shim, recognised by its sentinel bounds.
                    if a <= ANY_NUMBER_MIN && b >= ANY_NUMBER_MAX {
                        (ValidationRule::None, ctype)
                    } else {
                        (ValidationRule::Between { min: a, max: b }, ctype)
                    }
                }
                Some("notBetween") => (ValidationRule::NotBetween { min: a, max: b }, ctype),
                Some(op) => match CmpOp::from_xlsx(op) {
                    Some(op) => (ValidationRule::Compare { op, value: a }, ctype),
                    None => (ValidationRule::None, ctype),
                },
            }
        }
        _ => (ValidationRule::None, ColumnType::Any),
    };

    // The extension tag wins where it speaks: it carries the things xlsx
    // cannot express, and it was written by us, so it is more precise.
    let (rule, ctype, require_value) = match &tag {
        Some(t) => (
            t.rule.clone().unwrap_or(rule),
            t.ctype.unwrap_or(ctype),
            t.require_value,
        ),
        None => (rule, ctype, false),
    };

    let parsed = ParsedValidation {
        rule,
        ctype,
        allow_blank: p.allow_blank && !require_value,
        message: p.error.filter(|s| !s.is_empty()),
        tag,
    };
    for c in p.cols {
        out.validations.insert(c, parsed.clone());
    }
}

fn finish_cf(c: PartialCf, cols: &[u32], styles: &Styles, out: &mut SheetDecorations) {
    let rule = match c.kind.as_str() {
        "colorScale" => match c.colors.len() {
            2 => Some(ConditionalRule::ColorScale2 {
                min: c.colors[0],
                max: c.colors[1],
            }),
            n if n >= 3 => Some(ConditionalRule::ColorScale3 {
                min: c.colors[0],
                mid: c.colors[1],
                max: c.colors[2],
            }),
            _ => None,
        },
        "dataBar" => c
            .colors
            .first()
            .map(|color| ConditionalRule::DataBar { color: *color }),
        "cellIs" => {
            let op = c.operator.as_deref().and_then(CmpOp::from_xlsx);
            let value: Option<f64> = c.formula.as_deref().and_then(|s| s.trim().parse().ok());
            let (fill, text) = c
                .dxf
                .and_then(|i| styles.dxfs.get(i))
                .copied()
                .unwrap_or((None, None));
            match (op, value) {
                (Some(op), Some(value)) => Some(ConditionalRule::Threshold {
                    op,
                    value,
                    // Excel's default "bad" highlight, used when the dxf gave
                    // us nothing — better than inventing black-on-black.
                    fill: fill.unwrap_or(Rgb(255, 199, 206)),
                    text: text.unwrap_or(Rgb(156, 0, 6)),
                }),
                _ => None,
            }
        }
        _ => None,
    };
    let Some(rule) = rule else { return };
    for &col in cols {
        out.conditionals.entry(col).or_default().push(rule.clone());
    }
}

/// Parse `xl/tables/tableN.xml` into a bare [`Table`].
fn parse_table_part(xml: &[u8], path: &str, part: &str) -> Result<Table, XlsxError> {
    let mut name = String::new();
    let mut range = None;
    let mut header_rows = 1u32;
    let mut totals_rows = 0u32;
    let mut autofilter = false;
    let mut banded_rows = false;
    let mut banded_cols = false;
    let mut style: Option<String> = None;
    let mut columns: Vec<TableColumn> = Vec::new();

    safeguard::scan_part(xml, path, part, None, |ev| {
        use quick_xml::events::Event as E;
        if let E::Start(e) | E::Empty(e) = ev {
            match e.local_name().as_ref() {
                b"table" => {
                    name = attr(e, b"displayName")
                        .or_else(|| attr(e, b"name"))
                        .unwrap_or_else(|| "Table1".into());
                    range = attr(e, b"ref").and_then(|r| TableRange::from_a1(&r));
                    header_rows = attr(e, b"headerRowCount")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1);
                    totals_rows = attr(e, b"totalsRowCount")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                }
                b"autoFilter" => autofilter = true,
                b"tableStyleInfo" => {
                    style = attr(e, b"name").filter(|s| !s.is_empty());
                    banded_rows = attr(e, b"showRowStripes").as_deref() == Some("1");
                    banded_cols = attr(e, b"showColumnStripes").as_deref() == Some("1");
                }
                b"tableColumn" => {
                    let mut c = TableColumn::new(attr(e, b"name").unwrap_or_default());
                    c.totals_function = attr(e, b"totalsRowFunction");
                    c.totals_label = attr(e, b"totalsRowLabel");
                    columns.push(c);
                }
                _ => {}
            }
        }
        Ok(())
    })?;

    let Some(range) = range else {
        return Err(XlsxError::TableParse {
            path: "table part".into(),
            detail: "table element has no usable ref attribute".into(),
        });
    };

    let mut table = Table::new(name, range).with_columns(columns);
    table.header_row = header_rows > 0;
    table.totals_row = totals_rows > 0;
    table.autofilter = autofilter;
    table.banded_rows = banded_rows;
    table.banded_cols = banded_cols;
    table.style = style;
    Ok(table)
}

/// Fold the worksheet-level parts (validation, formats, conditional rules)
/// onto the table's columns.
fn apply_sheet_decorations(table: &mut Table, sheet: &SheetDecorations) {
    for i in 0..table.columns.len() {
        let sheet_col = table.sheet_col(i);
        if let Some(code) = sheet.col_formats.get(&sheet_col) {
            table.columns[i].format = NumberFormat::from_code(code);
        }
        if let Some(v) = sheet.validations.get(&sheet_col) {
            table.columns[i].ctype = v.ctype;
            table.columns[i].validation = Validation {
                rule: v.rule.clone(),
                allow_empty: v.allow_blank,
                message: v.message.clone(),
            };
            if let Some(tag) = &v.tag {
                table.columns[i].filter = tag.filter.clone();
            }
        }
        if let Some(rules) = sheet.conditionals.get(&sheet_col) {
            table.columns[i].conditional = rules.clone();
        }
    }
}

#[cfg(test)]
mod tests;
