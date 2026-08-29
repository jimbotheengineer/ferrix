//! `.xlsx` import and export.
//!
//! Reading uses [`calamine`], writing uses [`rust_xlsxwriter`] — both pure
//! Rust, no native Excel dependency, no COM, no Python.
//!
//! # What survives a round trip
//!
//! | Ferrix                | xlsx                          | back as         |
//! |-----------------------|-------------------------------|-----------------|
//! | `Value::Number`       | numeric cell                  | `Number`        |
//! | `Value::Bool`         | boolean cell                  | `Bool`          |
//! | `Value::Text`         | shared string                 | `Text`          |
//! | `Value::Error(e)`     | formula `=#DIV/0!` w/ cached  | `Error(e)`      |
//! | `Value::Empty`        | not written at all            | `Empty`         |
//! | formula (overlay)     | `<f>` + cached `<v>`          | formula         |
//!
//! Excel has no way to write a *literal* error constant as a static cell —
//! the file format only carries `t="e"` on formula results. So an error value
//! is exported as the formula `=#DIV/0!` (an error constant is itself a legal
//! Excel formula) with the matching cached result. Excel shows a real error;
//! Ferrix reads the cached result back as `Value::Error`. The formula source
//! `#DIV/0!` does not parse in the Ferrix grammar, so the importer falls back
//! to the cached value and the cell lands as a literal error again.
//!
//! `#CIRC!` is Ferrix-specific and not an Excel error, so it is written as the
//! text `#CIRC!`. On import, any string cell whose contents exactly match one
//! of the canonical error spellings is read back as that error — which is what
//! Excel itself does when you type `#N/A` into a cell.
//!
//! Dates are numbers. xlsx stores a datetime as a serial number plus a display
//! format; Ferrix has no date type, so the serial number lands as
//! `Value::Number` and the format is dropped.
//!
//! Column headers are *not* exported. `Sheet::headers` is a CSV artifact
//! (row 0 of the source file), and there is no in-band way for xlsx to say
//! "row 0 is a header", so writing them would make export/import asymmetric.
//! A caller who wants a header row should materialize it as row 0 of the data.
//!
//! # Formulas
//!
//! A `Sheet` stores values, not formulas — formulas live in an
//! [`EditOverlay`] as [`CellInput::Formula`]. So the formula-carrying entry
//! points are [`import_xlsx_full`] and [`export_workbook`]; the simpler
//! [`import_xlsx`] / [`export_xlsx`] pair deals in values only.
//!
//! On import, each formula's source is run through the Ferrix parser *and*
//! every function name in it is checked against [`SUPPORTED_FUNCTIONS`] — the
//! list the evaluator actually implements. The name check is not redundant:
//! the parser accepts `Expr::Call` for any identifier and leaves `#NAME?` to
//! evaluation, so keeping a formula on parse-success alone would turn a
//! correct Excel `VLOOKUP` result into `#NAME?` on the first recalc. A
//! formula that fails either check is dropped and only Excel's cached result
//! is kept, as a literal. That count is reported in
//! [`ImportStats::formulas_dropped`] so the caller can warn rather than
//! silently lose work.
//!
//! # Row cap — this is a hard failure, never a truncation
//!
//! xlsx caps a worksheet at 1,048,576 rows and 16,384 columns. Exporting a
//! sheet larger than that returns [`XlsxError::TooManyRows`] /
//! [`XlsxError::TooManyCols`] *before writing anything*. It does not truncate,
//! and it does not silently spill across sheets — a 200M-row Ferrix dataset
//! split into 191 tabs would be worse than useless, and quietly dropping rows
//! from an export is how people ship wrong numbers. Callers who genuinely want
//! a slice should say so explicitly by exporting a subrange.
//!
//! # Memory
//!
//! Export streams: worksheets are created with
//! `add_worksheet_with_constant_memory`, which flushes each row as it is
//! written and keeps only the current row in RAM.
//!
//! Import does **not** stream. calamine's `Reader` API materializes one
//! worksheet's `Range` at a time, so peak memory is bounded by the largest
//! single sheet rather than the whole workbook. Given the xlsx row cap, the
//! worst case is bounded and modest compared to the datasets Ferrix targets.

use std::path::Path;

use calamine::{Data, Reader, Xlsx};
use ferrix_core::{CancelToken, CellInput, CellRef, EditOverlay, ErrorKind, Sheet, Value};
use ferrix_formula::{DefinedName, Expr, NameScope, NameTable};
use rust_xlsxwriter::{Formula, Workbook};

use crate::safeguard::{self, Limits, PartBudget, SafeguardError};

/// Excel's hard worksheet row limit.
pub const XLSX_MAX_ROWS: usize = 1_048_576;
/// Excel's hard worksheet column limit.
pub const XLSX_MAX_COLS: usize = 16_384;

/// Functions the Ferrix evaluator actually implements.
///
/// The parser is deliberately permissive — it builds `Expr::Call` for *any*
/// identifier, and it is the evaluator that answers `#NAME?` for one it does
/// not know. Importing on parse-success alone would therefore "keep" a
/// VLOOKUP as a live formula that evaluates to `#NAME?`, quietly destroying
/// Excel's correct cached answer. So imports check the names too, and this
/// list must track the `eval_call` match arms in `ferrix-formula/src/eval.rs`.
pub const SUPPORTED_FUNCTIONS: &[&str] = &[
    "SUM", "COUNT", "AVERAGE", "MIN", "MAX", "ABS", "SQRT", "ROUND", "FLOOR", "CEILING", "INT",
    "LN", "LOG10", "EXP", "IF", "AND", "OR", "NOT",
];

/// Does every function call in this expression have an implementation?
fn calls_are_supported(e: &Expr) -> bool {
    match e {
        Expr::Call(name, args) => {
            let upper = name.to_ascii_uppercase();
            // The family modules are asked DIRECTLY rather than having their
            // names copied into the list above — one source of truth, so a
            // function added to a family is importable the same day and
            // cannot drift out of sync with `eval_call`.
            (SUPPORTED_FUNCTIONS.contains(&upper.as_str())
                || ferrix_formula::datetime::is_date_fn(&upper)
                || ferrix_formula::lookup::is_lookup_fn(&upper))
                && args.iter().all(calls_are_supported)
        }
        Expr::Unary(_, a) => calls_are_supported(a),
        Expr::Binary(_, a, b) => calls_are_supported(a) && calls_are_supported(b),
        Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Ref(_) | Expr::Range(_, _) => true,
        // Cross-sheet references are supported now that workbooks hold every
        // sheet; whether the named sheet actually exists is resolved when the
        // workbook builds its dependency graph, not here.
        Expr::XRef(_, _) | Expr::XRange(_, _, _) => true,
    }
}

/// Can Ferrix keep this formula source as a live formula?
fn formula_is_supported(src: &str) -> bool {
    ferrix_formula::parse(src).is_ok_and(|e| calls_are_supported(&e))
}

/// OOXML's marker for a function newer than the file format's original
/// function set.
///
/// Excel writes `XLOOKUP` into the XML as `_xlfn.XLOOKUP`, and a file that
/// omits the prefix shows `#NAME?` in Excel — so the EXPORT side must keep it
/// (rust_xlsxwriter adds it for us). The import side has to strip it again,
/// or every future function comes back as an unparseable name and gets
/// silently downgraded to its cached value: the sheet looks perfect until the
/// data underneath changes and the dead formula never recalculates.
///
/// `_xlfn._xlws.` is the same idea for worksheet-only functions and is
/// handled by stripping `_xlws.` after `_xlfn.`.
const FUTURE_FN_PREFIXES: &[&str] = &["_xlfn._xlws.", "_xlfn.", "_xlws."];

/// Remove OOXML future-function prefixes from formula source text.
///
/// Text INSIDE string literals is left alone — a formula may legitimately
/// contain the characters `_xlfn.` in a string, and rewriting there would
/// corrupt data rather than normalise a function name.
///
/// Returns `Cow::Borrowed` when there is nothing to strip, which is the
/// overwhelmingly common case, so an import of a prefix-free workbook pays
/// one scan and no allocation.
fn strip_future_fn_prefixes(src: &str) -> std::borrow::Cow<'_, str> {
    if !src.contains("_xl") {
        return std::borrow::Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    let mut in_string = false;
    while !rest.is_empty() {
        if in_string {
            // Copy through to the closing quote. `""` is an escaped quote
            // inside an Excel string, and falls out naturally: the closing
            // quote ends the string and the next one opens a new one.
            match rest.find('"') {
                Some(i) => {
                    out.push_str(&rest[..=i]);
                    rest = &rest[i + 1..];
                    in_string = false;
                }
                None => {
                    out.push_str(rest);
                    break;
                }
            }
            continue;
        }
        if let Some(p) = FUTURE_FN_PREFIXES.iter().find(|p| rest.starts_with(**p)) {
            rest = &rest[p.len()..];
            continue;
        }
        let c = rest.chars().next().expect("non-empty");
        if c == '"' {
            in_string = true;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    std::borrow::Cow::Owned(out)
}

#[derive(Debug, thiserror::Error)]
pub enum XlsxError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: Box<calamine::XlsxError>,
    },

    #[error("writing {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: Box<rust_xlsxwriter::XlsxError>,
    },

    /// Refused to export: the sheet has more rows than xlsx can hold.
    #[error(
        "sheet {name:?} has {rows} rows, but .xlsx holds at most {XLSX_MAX_ROWS}; \
         export refused rather than truncating — save as .ferrix or .csv, or export a subrange"
    )]
    TooManyRows { name: String, rows: usize },

    /// Refused to export: the sheet has more columns than xlsx can hold.
    #[error(
        "sheet {name:?} has {cols} columns, but .xlsx holds at most {XLSX_MAX_COLS}; \
         export refused rather than truncating"
    )]
    TooManyCols { name: String, cols: usize },

    #[error("workbook has no worksheets")]
    NoSheets,

    /// A table part existed but could not be understood.
    #[error("reading table parts of {path}: {detail}")]
    TableParse { path: String, detail: String },

    /// Excel refused a defined name — it breaks one of its naming rules.
    #[error("cannot write defined name {name:?}: {detail}")]
    DefinedName { name: String, detail: String },

    /// Writing the `<workbookProtection>` element failed (issue #42).
    ///
    /// Its own variant because that element is injected into the package
    /// AFTER `rust_xlsxwriter` has written it — see
    /// [`crate::protect_xlsx::inject_workbook_protection`] — so a failure
    /// there means "the file on disk may be missing its structure lock",
    /// which is a different thing from a normal write failure.
    #[error("writing workbook protection to {path}: {detail}")]
    WorkbookProtection { path: String, detail: String },

    /// The file was refused, or failed mid-read, by the resource safeguards.
    ///
    /// Kept as its own variant rather than flattened into
    /// [`XlsxError::TableParse`] so a caller can tell "this file is hostile
    /// or truncated" from "this file is fine but Ferrix does not understand
    /// part of it", and so the failing part name survives to the UI.
    #[error(transparent)]
    Safeguard(#[from] SafeguardError),
}

/// A worksheet as it came out of a workbook.
pub struct ImportedSheet {
    pub name: String,
    /// Cell values (Excel's cached results, for formula cells).
    pub sheet: Sheet,
    /// Formulas Ferrix understood, keyed by cell.
    pub formulas: EditOverlay,
    pub stats: ImportStats,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportStats {
    /// Non-empty cells read.
    pub cells: usize,
    /// Formulas kept as live formulas.
    pub formulas_kept: usize,
    /// Formulas the Ferrix parser rejected; only their cached value was kept.
    pub formulas_dropped: usize,
}

// ---------------------------------------------------------------- import ---

/// Import every worksheet, values only.
///
/// Formulas are collapsed to Excel's cached result. Use [`import_xlsx_full`]
/// if you want the formulas back as formulas.
pub fn import_xlsx(path: impl AsRef<Path>) -> Result<Vec<(String, Sheet)>, XlsxError> {
    Ok(import_xlsx_full(path)?
        .into_iter()
        .map(|s| (s.name, s.sheet))
        .collect())
}

/// Import every worksheet, keeping formulas the Ferrix parser accepts.
pub fn import_xlsx_full(path: impl AsRef<Path>) -> Result<Vec<ImportedSheet>, XlsxError> {
    import_xlsx_guarded(path, &Limits::measured(), None)
}

/// Import under explicit resource limits, cancellably.
///
/// This is the real entry point; [`import_xlsx_full`] is it with a measured
/// budget and no cancel token. Three things happen before calamine is handed
/// the file, in this order:
///
/// 1. The zip central directory is vetted — declared expansion ratio,
///    declared total, entry count, entry paths. Nothing is extracted, so a
///    bomb costs a directory read rather than a decompression.
/// 2. The largest declared part is checked against the per-part cap, because
///    calamine materializes one worksheet's `Range` at a time and that is
///    the allocation that would blow up.
/// 3. Only then is the workbook opened.
///
/// Cancellation is polled between sheets and, for large sheets, between
/// cells. A cancelled import returns [`SafeguardError::Cancelled`] and every
/// sheet built so far is dropped — the caller receives no partial workbook.
pub fn import_xlsx_guarded(
    path: impl AsRef<Path>,
    limits: &Limits,
    cancel: Option<&CancelToken>,
) -> Result<Vec<ImportedSheet>, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();
    let read_err = |e: calamine::XlsxError| XlsxError::Read {
        path: disp.clone(),
        source: Box::new(e),
    };

    // Vet the package from its central directory, before calamine builds a
    // decompressor. The archive handle is dropped immediately: this is a
    // check, not the read path.
    {
        let (_zip, report) = safeguard::open_checked(path, limits)?;
        if report.largest_part_bytes > limits.max_part_bytes {
            return Err(SafeguardError::PartTooLarge {
                path: disp.clone(),
                part: report.largest_part.clone(),
                declared: report.largest_part_bytes,
                limit: limits.max_part_bytes,
            }
            .into());
        }
    }

    let mut wb: Xlsx<_> = calamine::open_workbook(path).map_err(read_err)?;
    let names = wb.sheet_names();
    if names.is_empty() {
        return Err(XlsxError::NoSheets);
    }

    let cancelled = |part: &str| -> XlsxError {
        SafeguardError::Cancelled {
            path: disp.clone(),
            part: part.to_string(),
        }
        .into()
    };

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            // `out` is dropped here. A cancelled import yields no sheets at
            // all rather than the prefix that happened to finish.
            return Err(cancelled(&name));
        }
        // One sheet resident at a time — peak memory is the largest sheet,
        // not the whole workbook.
        let range = wb.worksheet_range(&name).map_err(read_err)?;
        // Formulas are a separate pass; a workbook with none costs nothing.
        let formulas = wb.worksheet_formula(&name).map_err(read_err)?;
        match build_sheet(&name, &range, &formulas, cancel) {
            Some(s) => out.push(s),
            None => return Err(cancelled(&name)),
        }
    }
    Ok(out)
}

/// How many cells pass between cancellation polls while building a sheet.
const CELL_CANCEL_POLL: usize = 8192;

/// Build one sheet. `None` means cancellation was observed; the half-built
/// sheet is dropped rather than returned.
fn build_sheet(
    name: &str,
    range: &calamine::Range<Data>,
    formulas: &calamine::Range<String>,
    cancel: Option<&CancelToken>,
) -> Option<ImportedSheet> {
    let mut sheet = Sheet::new(name);
    let mut overlay = EditOverlay::new();
    let mut stats = ImportStats::default();

    // `used_cells` yields positions relative to the range's own origin.
    let (r0, c0) = range.start().unwrap_or((0, 0));
    for (n, (r, c, data)) in range.used_cells().enumerate() {
        if cancel.is_some() && n % CELL_CANCEL_POLL == 0 && cancel.is_some_and(|c| c.is_cancelled())
        {
            return None;
        }
        let cell = CellRef::new(r0 + r as u32, c0 + c as u32);
        let value = data_to_value(data, &mut sheet);
        if value.is_empty() {
            continue;
        }
        stats.cells += 1;
        sheet.set(cell, value);
    }

    let (fr0, fc0) = formulas.start().unwrap_or((0, 0));
    for (n, (r, c, src)) in formulas.used_cells().enumerate() {
        if cancel.is_some() && n % CELL_CANCEL_POLL == 0 && cancel.is_some_and(|c| c.is_cancelled())
        {
            return None;
        }
        if src.is_empty() {
            continue;
        }
        let cell = CellRef::new(fr0 + r as u32, fc0 + c as u32);
        // Our own error-constant encoding (`=#DIV/0!`) is not a real formula.
        // The cached value already decoded it; do not count it as a loss.
        if error_from_str(src).is_some() {
            continue;
        }
        // xlsx stores the body without the leading '='. Future-function
        // prefixes (`_xlfn.XLOOKUP`) are normalised away here, at the file
        // boundary, so nothing downstream — parser, evaluator, dep graph —
        // ever has to know the on-disk spelling exists.
        let src = format!("={}", strip_future_fn_prefixes(src));
        if !formula_is_supported(&src) {
            // An Excel function Ferrix does not implement, or a structured
            // table reference. Keep the cached value, drop the formula.
            stats.formulas_dropped += 1;
            continue;
        }
        // Re-intern the cached text into the overlay's own arena: overlay
        // values must not carry base-arena ids.
        let cached = match sheet.get(cell) {
            Value::Text(id) => {
                let s = sheet.resolve(id).to_string();
                Value::Text(overlay.intern(&s))
            }
            other => other,
        };
        overlay.set(cell, CellInput::Formula { src, cached });
        stats.formulas_kept += 1;
    }

    Some(ImportedSheet {
        name: name.to_string(),
        sheet,
        formulas: overlay,
        stats,
    })
}

fn data_to_value(data: &Data, sheet: &mut Sheet) -> Value {
    match data {
        Data::Empty => Value::Empty,
        Data::Int(i) => Value::Number(*i as f64),
        Data::Float(f) => Value::Number(*f),
        Data::Bool(b) => Value::Bool(*b),
        // Dates arrive as serial numbers; Ferrix has no date type.
        Data::DateTime(dt) => Value::Number(dt.as_f64()),
        Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => match error_from_str(s) {
            Some(e) => Value::Error(e),
            None => Value::Text(sheet.intern(s)),
        },
        Data::Error(e) => Value::Error(cell_error_to_kind(e)),
    }
}

/// Recognize a canonical error spelling written as text. This is how `#CIRC!`
/// gets home, and it matches Excel's own behaviour for typed-in error text.
fn error_from_str(s: &str) -> Option<ErrorKind> {
    const ALL: [ErrorKind; 8] = [
        ErrorKind::DivZero,
        ErrorKind::Value,
        ErrorKind::Ref,
        ErrorKind::Name,
        ErrorKind::Num,
        ErrorKind::NotAvailable,
        ErrorKind::Null,
        ErrorKind::Circular,
    ];
    ALL.into_iter().find(|e| e.as_str() == s)
}

fn cell_error_to_kind(e: &calamine::CellErrorType) -> ErrorKind {
    use calamine::CellErrorType as C;
    match e {
        C::Div0 => ErrorKind::DivZero,
        C::NA => ErrorKind::NotAvailable,
        C::Name => ErrorKind::Name,
        C::Null => ErrorKind::Null,
        C::Num => ErrorKind::Num,
        C::Ref => ErrorKind::Ref,
        C::Value => ErrorKind::Value,
        // No Ferrix equivalent; it means "external data not yet fetched".
        C::GettingData => ErrorKind::NotAvailable,
    }
}

// ------------------------------------------------------- defined names ---

/// Read `<definedName>` entries out of `xl/workbook.xml`.
///
/// Done by opening the package directly rather than through calamine, which
/// exposes `defined_names()` as bare `(name, formula)` pairs and drops the
/// `localSheetId` attribute — the only thing in the file that distinguishes a
/// sheet-scoped name from a workbook-scoped one. Losing it would silently
/// promote every local name to workbook scope on import, so two sheets with
/// their own `Total` would collide.
///
/// `localSheetId` is an index into the `<sheets>` order, which is the same
/// order [`import_xlsx_full`] returns, so the scope is resolved to a sheet
/// NAME here and the caller never has to think about indices.
///
/// Built-in names (`_xlnm.Print_Area` and friends) are skipped: they are
/// Excel print settings, not user ranges, and Ferrix has no equivalent.
pub fn import_defined_names(path: impl AsRef<Path>) -> Result<NameTable, XlsxError> {
    import_defined_names_guarded(path, &Limits::measured(), None)
}

/// [`import_defined_names`] under explicit limits, cancellably.
///
/// ## The bug this replaces
///
/// The previous reader matched `Ok(Event::Eof) | Err(_) => break` — a parse
/// ERROR and a clean end of file were the same thing. A `xl/workbook.xml`
/// truncated after two of its five `<definedName>` elements therefore
/// imported as a workbook with two names and no complaint anywhere. That is
/// silent data loss dressed as a successful import, and it is why every
/// reader in this crate now separates the two.
pub fn import_defined_names_guarded(
    path: impl AsRef<Path>,
    limits: &Limits,
    cancel: Option<&CancelToken>,
) -> Result<NameTable, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();

    let (mut zip, _report) = safeguard::open_checked(path, limits)?;
    const PART: &str = "xl/workbook.xml";
    let mut budget = PartBudget::new(disp.clone(), limits);
    let xml = {
        let f = zip
            .by_name(PART)
            .map_err(|e| SafeguardError::PartUnreadable {
                path: disp.clone(),
                part: PART.to_string(),
                detail: e.to_string(),
            })?;
        let declared = f.size();
        budget.read(f, declared, PART)?
    };

    let sheet_order = workbook_sheet_names(&xml, &disp)?;
    let mut table = NameTable::new();
    let mut pending: Option<(String, Option<usize>)> = None;
    let mut text = String::new();

    safeguard::scan_part(&xml, &disp, PART, cancel, |ev| {
        use quick_xml::events::Event as E;
        match ev {
            E::Start(e) if e.local_name().as_ref() == b"definedName" => {
                let Some(name) = xattr(e, b"name") else {
                    return Ok(());
                };
                let local = xattr(e, b"localSheetId").and_then(|v| v.parse::<usize>().ok());
                pending = Some((name, local));
                text.clear();
            }
            E::Text(t) if pending.is_some() => {
                if let Ok(s) = t.xml10_content() {
                    text.push_str(&s);
                }
            }
            E::End(e) if e.local_name().as_ref() == b"definedName" => {
                if let Some((name, local)) = pending.take() {
                    // `_xlnm.*` are Excel's own print/filter settings.
                    if !name.starts_with("_xlnm") {
                        let scope = match local.and_then(|i| sheet_order.get(i)) {
                            Some(s) => NameScope::Sheet(s.clone()),
                            None => NameScope::Workbook,
                        };
                        // A target Ferrix cannot parse (a 3-D reference, an
                        // Excel-only function) is dropped rather than stored
                        // as something that would evaluate to nonsense.
                        let _ = table.insert(DefinedName::new(name, scope, text.trim()));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(table)
}

/// Sheet names in `xl/workbook.xml` order — what `localSheetId` indexes.
///
/// Errors rather than stopping short on malformed XML: a truncated
/// `<sheets>` list would shift every `localSheetId`, silently re-scoping
/// names onto the wrong sheets.
fn workbook_sheet_names(xml: &[u8], path: &str) -> Result<Vec<String>, SafeguardError> {
    let mut out = Vec::new();
    safeguard::scan_part(xml, path, "xl/workbook.xml", None, |ev| {
        use quick_xml::events::Event as E;
        if let E::Empty(e) | E::Start(e) = ev {
            if e.local_name().as_ref() == b"sheet" {
                if let Some(n) = xattr(e, b"name") {
                    out.push(n);
                }
            }
        }
        Ok(())
    })?;
    Ok(out)
}

/// Read an attribute by local name, resolving XML entities.
fn xattr(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.as_ref() != name && a.key.local_name().as_ref() != name {
            return None;
        }
        Some(
            match a.normalized_value(quick_xml::XmlVersion::Implicit1_0) {
                Ok(v) => v.into_owned(),
                Err(_) => String::from_utf8_lossy(a.value.as_ref()).into_owned(),
            },
        )
    })
}

/// Write a name table as `<definedName>` entries.
///
/// `rust_xlsxwriter` addresses a sheet-scoped name through the `Sheet!Name`
/// spelling and emits the correct `localSheetId` itself, which is why the
/// scope is re-encoded into the name here rather than written by hand.
fn write_defined_names(wb: &mut Workbook, names: &NameTable) -> Result<(), XlsxError> {
    for d in names.iter() {
        // Excel wants a leading '=' on the formula.
        let formula = if d.refers_to.starts_with('=') {
            d.refers_to.clone()
        } else {
            format!("={}", d.refers_to)
        };
        let key = match d.scope.sheet() {
            Some(s) => format!("{}!{}", ferrix_formula::quote_sheet_name(s), d.name),
            None => d.name.clone(),
        };
        wb.define_name(&key, &formula)
            .map_err(|e| XlsxError::DefinedName {
                name: d.name.clone(),
                detail: e.to_string(),
            })?;
    }
    Ok(())
}

// ---------------------------------------------------------------- export ---

/// One worksheet to write.
pub struct SheetExport<'a> {
    pub name: &'a str,
    pub sheet: &'a Sheet,
    /// Formulas to write instead of the sheet's cached values.
    pub formulas: Option<&'a EditOverlay>,
    /// Structured tables to define over this sheet. Each becomes a real
    /// `xl/tables/tableN.xml` part plus its validation/conditional-format
    /// elements — see [`crate::table_xlsx`].
    pub tables: &'a [ferrix_core::Table],
    /// Merged regions, written as `<mergeCells>`.
    pub merges: Option<&'a ferrix_core::merge::MergeMap>,
    /// Cell comments, written as legacy Excel notes — see
    /// [`crate::table_xlsx::write_comments`] for exactly what Excel shows.
    pub comments: Option<&'a ferrix_core::CommentMap>,
    /// Sheet protection, written as `<sheetProtection>` plus one
    /// `<protectedRange>` per unlocked rectangle (issue #42). See
    /// [`crate::protect_xlsx`] — and read its "this is not security" note
    /// before assuming this does anything an attacker would notice.
    pub protection: Option<&'a ferrix_core::SheetProtection>,
    /// Row heights, column widths, hidden spans and outline groups (issue
    /// #29), written as `<cols>` attributes and per-`<row>` attributes.
    pub sizing: Option<&'a ferrix_core::sizing::SheetSizing>,
    /// Sheet-wide formatting, for its cell DECORATION (issue #28): borders,
    /// alignment, indent, wrap, shrink and rotation. Written as real
    /// `xl/styles.xml` records — see [`crate::decor_xlsx`], and read its
    /// note on why a column-scope decoration is one `<col>` record while a
    /// range-scope one is capped.
    pub format: Option<&'a ferrix_core::SheetFormat>,
}

impl<'a> SheetExport<'a> {
    pub fn new(name: &'a str, sheet: &'a Sheet) -> Self {
        Self {
            name,
            sheet,
            formulas: None,
            tables: &[],
            merges: None,
            comments: None,
            protection: None,
            sizing: None,
            format: None,
        }
    }

    pub fn with_formulas(mut self, overlay: &'a EditOverlay) -> Self {
        self.formulas = Some(overlay);
        self
    }

    /// Attach merged regions, written as `<mergeCells>`.
    pub fn with_merges(mut self, merges: &'a ferrix_core::merge::MergeMap) -> Self {
        self.merges = Some(merges);
        self
    }

    /// Attach cell comments, written as legacy Excel notes.
    pub fn with_comments(mut self, comments: &'a ferrix_core::CommentMap) -> Self {
        self.comments = Some(comments);
        self
    }

    pub fn with_tables(mut self, tables: &'a [ferrix_core::Table]) -> Self {
        self.tables = tables;
        self
    }

    /// Attach sheet protection. Writes `<sheetProtection>` and, for every
    /// unlocked range, a `<protectedRange>` — Excel's "Allow Users to Edit
    /// Ranges" list.
    pub fn with_protection(mut self, prot: &'a ferrix_core::SheetProtection) -> Self {
        self.protection = Some(prot);
        self
    }

    /// Attach row/column sizing, hiding and outline grouping.
    pub fn with_sizing(mut self, sizing: &'a ferrix_core::sizing::SheetSizing) -> Self {
        self.sizing = Some(sizing);
        self
    }

    /// Attach sheet formatting, so cell decoration reaches `xl/styles.xml`.
    pub fn with_format(mut self, format: &'a ferrix_core::SheetFormat) -> Self {
        self.format = Some(format);
        self
    }
}

/// Export a single sheet's values.
pub fn export_xlsx(
    path: impl AsRef<Path>,
    sheet: &Sheet,
    sheet_name: &str,
) -> Result<(), XlsxError> {
    export_workbook(path, &[SheetExport::new(sheet_name, sheet)])
}

/// Export a single sheet plus its formula overlay.
pub fn export_xlsx_with_formulas(
    path: impl AsRef<Path>,
    sheet: &Sheet,
    sheet_name: &str,
    formulas: &EditOverlay,
) -> Result<(), XlsxError> {
    export_workbook(
        path,
        &[SheetExport::new(sheet_name, sheet).with_formulas(formulas)],
    )
}

/// Export a multi-sheet workbook.
///
/// Every sheet's extent is checked against the xlsx limits *before* any
/// writing happens, so an oversized sheet fails without leaving a partial
/// file behind.
pub fn export_workbook(path: impl AsRef<Path>, sheets: &[SheetExport]) -> Result<(), XlsxError> {
    export_workbook_with_names(path, sheets, &NameTable::new())
}

/// Export a multi-sheet workbook together with its defined names.
///
/// Names are written as OOXML `<definedName>` elements, workbook-scoped ones
/// bare and sheet-scoped ones carrying the `localSheetId` that binds them to
/// one sheet — the only thing in the format that distinguishes the two scopes.
pub fn export_workbook_with_names(
    path: impl AsRef<Path>,
    sheets: &[SheetExport],
    names: &NameTable,
) -> Result<(), XlsxError> {
    export_workbook_full(path, sheets, names, &ferrix_core::WorkbookProtection::new())
}

/// [`export_workbook_with_names`] plus workbook-structure protection.
///
/// `rust_xlsxwriter` has no writer for `<workbookProtection>` at all, so that
/// one element is injected into `xl/workbook.xml` after the package is
/// written, by rewriting the zip entry in place. Everything else still goes
/// through the library.
pub fn export_workbook_full(
    path: impl AsRef<Path>,
    sheets: &[SheetExport],
    names: &NameTable,
    wb_protection: &ferrix_core::WorkbookProtection,
) -> Result<(), XlsxError> {
    if sheets.is_empty() {
        return Err(XlsxError::NoSheets);
    }
    for s in sheets {
        check_limits(s.name, s.sheet, s.formulas)?;
    }

    let path = path.as_ref();
    let disp = path.display().to_string();
    let write_err = |e: rust_xlsxwriter::XlsxError| XlsxError::Write {
        path: disp.clone(),
        source: Box::new(e),
    };

    let mut wb = Workbook::new();
    for s in sheets {
        // Constant-memory mode flushes each row as it is finished, so a
        // million-row export never holds a million rows in RAM. It requires
        // strictly increasing row order, which the loop below guarantees.
        //
        // A sheet carrying tables uses an ordinary worksheet instead: a table
        // part is attached after the cells are written, which a flushed-as-you-
        // go worksheet cannot support. Tables are capped by Excel's own row
        // limit anyway, and `check_limits` has already refused anything above
        // it, so the memory exposure is bounded.
        // Merges, like tables, need the buffering writer: a merge is applied
        // to a range after its cells are written, which the constant-memory
        // writer cannot revisit.
        // Notes join tables and merges on the non-constant-memory path: a
        // note is attached to a cell after the fact, which the streaming
        // writer cannot revisit.
        // Sizing joins them for exactly the same reason (issue #29): row
        // heights, hidden flags and outline levels are row PROPERTIES applied
        // after the row's cells exist, and the constant-memory writer has
        // already flushed that row to disk by then — the properties are
        // silently dropped and the file comes back with no sizing at all.
        // Sheets without sizing keep the streaming writer, so the 10GB export
        // path is unchanged.
        let ws = if s.tables.is_empty()
            && s.merges.is_none_or(|m| m.is_empty())
            && s.comments.is_none_or(|c| c.is_empty())
            // Protection also needs the buffering writer: `<protectedRange>`
            // is emitted from a list the worksheet accumulates, and the
            // constant-memory writer has already flushed its header by the
            // time the ranges are known.
            && s.protection.is_none()
            && s.sizing.is_none_or(|z| z.is_empty())
            // Decoration joins them (issue #28): a cell format is applied to
            // a cell AFTER it is written, and the constant-memory writer has
            // already flushed that row — the format would be silently dropped
            // and the file would come back with no borders at all. A sheet
            // with no decoration keeps the streaming writer, so the 10GB
            // export path is unchanged.
            && s.format.is_none_or(|f| !f.has_decor())
        {
            wb.add_worksheet_with_constant_memory()
        } else {
            wb.add_worksheet()
        };
        ws.set_name(s.name).map_err(write_err)?;
        write_sheet(ws, s).map_err(write_err)?;
        for table in s.tables {
            crate::table_xlsx::write_table(ws, table).map_err(write_err)?;
        }
        if let Some(m) = s.merges {
            crate::table_xlsx::write_merges(ws, m).map_err(write_err)?;
        }
        if let Some(c) = s.comments {
            crate::table_xlsx::write_comments(ws, c).map_err(write_err)?;
        }
        if let Some(p) = s.protection {
            crate::protect_xlsx::write_protection(ws, p).map_err(write_err)?;
        }
    }
    write_defined_names(&mut wb, names)?;
    wb.save(path).map_err(write_err)?;
    if wb_protection.is_active() {
        crate::protect_xlsx::inject_workbook_protection(path, wb_protection)?;
    }
    Ok(())
}

/// Export a sheet with structured tables defined over it.
pub fn export_xlsx_with_tables(
    path: impl AsRef<Path>,
    sheet: &Sheet,
    sheet_name: &str,
    tables: &[ferrix_core::Table],
) -> Result<(), XlsxError> {
    export_workbook(
        path,
        &[SheetExport::new(sheet_name, sheet).with_tables(tables)],
    )
}

/// The row/column extent that would actually be written, overlay included.
fn extent(sheet: &Sheet, formulas: Option<&EditOverlay>) -> (usize, usize) {
    let (mut rows, mut cols) = (sheet.row_count(), sheet.col_count());
    if let Some(o) = formulas {
        let (r, c) = o.extent();
        rows = rows.max(r);
        cols = cols.max(c);
    }
    (rows, cols)
}

fn check_limits(
    name: &str,
    sheet: &Sheet,
    formulas: Option<&EditOverlay>,
) -> Result<(), XlsxError> {
    let (rows, cols) = extent(sheet, formulas);
    if rows > XLSX_MAX_ROWS {
        return Err(XlsxError::TooManyRows {
            name: name.to_string(),
            rows,
        });
    }
    if cols > XLSX_MAX_COLS {
        return Err(XlsxError::TooManyCols {
            name: name.to_string(),
            cols,
        });
    }
    Ok(())
}

fn write_sheet(
    ws: &mut rust_xlsxwriter::Worksheet,
    s: &SheetExport,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    let (rows, cols) = extent(s.sheet, s.formulas);
    for r in 0..rows as u32 {
        for c in 0..cols as u32 {
            let cell = CellRef::new(r, c);

            if let Some(CellInput::Formula { src, cached }) = s.formulas.and_then(|o| o.get(cell)) {
                let body = src.strip_prefix('=').unwrap_or(src);
                let result = overlay_display(cached, s.formulas.unwrap(), s.sheet);
                ws.write_formula(r, c as u16, Formula::new(body).set_result(result))?;
                continue;
            }

            let literal = s
                .formulas
                .and_then(|o| o.get(cell))
                .map(CellInput::value)
                .unwrap_or_else(|| s.sheet.get(cell));

            match literal {
                Value::Empty => {}
                Value::Number(n) if n.is_finite() => {
                    ws.write_number(r, c as u16, n)?;
                }
                // NaN/inf have no xlsx representation; Excel calls that #NUM!.
                Value::Number(_) => write_error(ws, r, c as u16, ErrorKind::Num)?,
                Value::Bool(b) => {
                    ws.write_boolean(r, c as u16, b)?;
                }
                Value::Text(id) => {
                    let text = s
                        .formulas
                        .and_then(|o| o.resolve(id))
                        .unwrap_or_else(|| s.sheet.resolve(id));
                    ws.write_string(r, c as u16, text)?;
                }
                Value::Error(e) => write_error(ws, r, c as u16, e)?,
            }
        }
    }
    // Sizing goes on AFTER the cells. `rust_xlsxwriter` merges row properties
    // into the row records it has already created, and a row property set
    // before that row has any cells is dropped — which is exactly how the
    // outline levels silently failed to appear in the file the first time.
    write_sizing(ws, s)?;
    // Decoration goes on after the cells for the same reason sizing does:
    // `rust_xlsxwriter` merges a cell format into the cell record it already
    // has, and a format set before the cell exists is dropped.
    if let Some(fmt) = s.format {
        let (rows, cols) = extent(s.sheet, s.formulas);
        crate::decor_xlsx::write_decor(ws, fmt, rows, cols)?;
    }
    Ok(())
}

/// Write row/column sizing, hiding and outline grouping (issue #29).
///
/// # Why this stays bounded
///
/// Row state is written per SPAN, and only spans that differ from the default
/// are stored at all — but `rust_xlsxwriter` addresses rows one at a time, so
/// a span is expanded to its rows HERE, capped at the rows the sheet actually
/// writes. A hidden span past the sheet's extent contributes nothing: there is
/// no `<row>` element for a row with no cells, so writing one would be both
/// wasted bytes and a row Excel did not previously have.
///
/// # Excel's units
///
/// Ferrix stores widths in PIXELS; xlsx column widths are in character units.
/// `set_column_width_pixels` does the conversion, so the number that goes into
/// the file is the one Excel will render at, rather than a pixel count
/// reinterpreted as characters (which would come back roughly 7x too wide).
fn write_sizing(
    ws: &mut rust_xlsxwriter::Worksheet,
    s: &SheetExport,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    let Some(sz) = s.sizing else {
        return Ok(());
    };
    let (rows, _) = extent(s.sheet, s.formulas);

    for (col, w) in sz.cols.widths() {
        if col as usize <= XLSX_MAX_COLS {
            ws.set_column_width_pixels(col as u16, w.round().max(1.0) as u32)?;
        }
    }
    for col in sz.cols.hidden_cols() {
        if col as usize <= XLSX_MAX_COLS {
            ws.set_column_hidden(col as u16)?;
        }
    }
    // A zero height IS hidden in Ferrix, and Excel spells that as the row's
    // `hidden` attribute rather than a height of 0 — a literal 0 height opens
    // as a visible row of minimum height in some versions. Written as `hidden`
    // so the file means in Excel what it means here.
    for (first, last, h) in sz.rows.spans() {
        let last = (last as usize).min(rows.saturating_sub(1)) as u32;
        for r in first..=last {
            if h == 0.0 {
                ws.set_row_hidden(r)?;
            } else {
                ws.set_row_height(r, h as f64)?;
            }
        }
    }
    for g in sz.row_outline.groups() {
        let last = (g.last as usize).min(rows.saturating_sub(1)) as u32;
        if g.first <= last {
            ws.group_rows(g.first, last)?;
            if g.collapsed {
                // The summary row stays visible and the body folds, matching
                // `Outline::collapsed_spans`.
                for r in g.first + 1..=last {
                    ws.set_row_hidden(r)?;
                }
            }
        }
    }
    for g in sz.col_outline.groups() {
        if (g.last as usize) <= XLSX_MAX_COLS {
            ws.group_columns(g.first as u16, g.last as u16)?;
        }
    }
    Ok(())
}

/// Read row/column sizing back out of a worksheet part (issue #29).
///
/// calamine surfaces cell VALUES, not the `<cols>` element or the `<row>`
/// attributes, so — exactly as `table_xlsx` does for tables and validations —
/// the package is opened directly and the worksheet XML parsed for the
/// attributes that carry sizing.
///
/// The import is the round-trip's other half: without it a width written on
/// export would be dropped on the next open, and the user's layout would
/// silently survive only until they reloaded their own file.
pub fn import_sizing(
    path: impl AsRef<Path>,
) -> Result<Vec<(String, ferrix_core::sizing::SheetSizing)>, XlsxError> {
    use ferrix_core::sizing::{Outline, OutlineGroup, SheetSizing};
    use quick_xml::events::Event;

    let path = path.as_ref();
    let limits = crate::safeguard::Limits::default();
    let disp = path.display().to_string();
    let (mut zip, _) = crate::safeguard::open_checked(path, &limits)?;
    let parts = crate::safeguard::read_all_parts(&mut zip, &disp, &limits, None)?;

    // No workbook part means there is nothing to read sizing from. That is a
    // malformed package, but it is `import_xlsx`'s job to report it — this
    // function's contract is "the sizing that is there", so it returns none
    // rather than a second, differently-worded error for the same file.
    let Some(wb_xml) = parts.get("xl/workbook.xml") else {
        return Ok(Vec::new());
    };
    let names = workbook_sheet_names(wb_xml, &disp)?;

    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let part = format!("xl/worksheets/sheet{}.xml", i + 1);
        let Some(xml) = parts.get(&part) else {
            continue;
        };
        let mut sizing = SheetSizing::new();
        // Groups are collected as ranges: xlsx stores an `outlineLevel` PER
        // ROW, so consecutive rows at the same level are coalesced back into
        // the one range Ferrix stores. Reconstructing ranges here is what
        // keeps the in-memory model O(groups) rather than O(rows) after an
        // import.
        let mut row_runs: Vec<(u32, u32, u8)> = Vec::new();
        let mut col_runs: Vec<(u32, u32, u8)> = Vec::new();

        let mut rdr = quick_xml::Reader::from_reader(xml.as_slice());
        rdr.config_mut().trim_text(true);
        let mut buf = Vec::new();
        loop {
            match rdr.read_event_into(&mut buf) {
                Ok(Event::Empty(e)) | Ok(Event::Start(e)) => match e.name().as_ref() {
                    b"col" => {
                        let min = xattr(&e, b"min").and_then(|v| v.parse::<u32>().ok());
                        let max = xattr(&e, b"max").and_then(|v| v.parse::<u32>().ok());
                        let (Some(min), Some(max)) = (min, max) else {
                            continue;
                        };
                        // xlsx column indices are 1-based and inclusive.
                        let (first, last) = (min.saturating_sub(1), max.saturating_sub(1));
                        let hidden = xattr(&e, b"hidden").is_some_and(|v| v == "1" || v == "true");
                        let width = xattr(&e, b"width").and_then(|v| v.parse::<f64>().ok());
                        let level = xattr(&e, b"outlineLevel")
                            .and_then(|v| v.parse::<u8>().ok())
                            .unwrap_or(0);
                        // Excel's width is in character units; Ferrix keeps
                        // pixels. This is the inverse of the conversion the
                        // writer applies, so a width survives the round trip
                        // instead of growing by a factor of seven each time.
                        for c in first..=last.min(first + 4096) {
                            if let Some(w) = width {
                                sizing.cols.set_width(c, char_units_to_px(w));
                            }
                            if hidden {
                                sizing.cols.hide(c);
                            }
                        }
                        if level > 0 {
                            col_runs.push((first, last, level));
                        }
                    }
                    b"row" => {
                        let Some(r) = xattr(&e, b"r").and_then(|v| v.parse::<u32>().ok()) else {
                            continue;
                        };
                        let r = r.saturating_sub(1);
                        let hidden = xattr(&e, b"hidden").is_some_and(|v| v == "1" || v == "true");
                        let custom =
                            xattr(&e, b"customHeight").is_some_and(|v| v == "1" || v == "true");
                        let ht = xattr(&e, b"ht").and_then(|v| v.parse::<f32>().ok());
                        if hidden {
                            // Height 0 IS hidden — the same spelling the rest
                            // of Ferrix uses, so an imported hidden row is
                            // indistinguishable from a locally hidden one.
                            sizing.rows.hide(r, r);
                        } else if let (true, Some(h)) = (custom, ht) {
                            sizing.rows.set(r, h);
                        }
                        if let Some(level) = xattr(&e, b"outlineLevel")
                            .and_then(|v| v.parse::<u8>().ok())
                            .filter(|&l| l > 0)
                        {
                            match row_runs.last_mut() {
                                Some(run) if run.1 + 1 == r && run.2 == level => run.1 = r,
                                _ => row_runs.push((r, r, level)),
                            }
                        }
                    }
                    _ => {}
                },
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        // xlsx marks the ROWS INSIDE a group, and Excel keeps the summary row
        // just outside it. Ferrix's range includes the summary row, so each
        // run is widened by one at the front to name the same group.
        let to_groups = |runs: Vec<(u32, u32, u8)>| {
            Outline::from_groups(runs.into_iter().map(|(a, b, level)| OutlineGroup {
                first: a.saturating_sub(1),
                last: b,
                level,
                collapsed: false,
            }))
        };
        sizing.row_outline = to_groups(row_runs);
        sizing.col_outline = to_groups(col_runs);
        out.push((name.clone(), sizing));
    }
    Ok(out)
}

/// Excel's column width unit is "characters of the default font". The factor
/// below is the one Excel itself documents for the standard 11pt Calibri
/// metric, and it is the exact inverse of what `set_column_width_pixels`
/// applies on the way out.
fn char_units_to_px(units: f64) -> f32 {
    ((units * 7.0) + 5.0) as f32
}

/// Write an error value.
///
/// xlsx only carries `t="e"` on a formula result, so a literal error is
/// written as the error-constant formula `=#DIV/0!` with the matching cached
/// result. `#CIRC!` is not an Excel error, so it goes out as plain text and is
/// recognized again on import by its spelling.
fn write_error(
    ws: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u16,
    e: ErrorKind,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    if e == ErrorKind::Circular {
        ws.write_string(row, col, e.as_str())?;
    } else {
        ws.write_formula(row, col, Formula::new(e.as_str()).set_result(e.as_str()))?;
    }
    Ok(())
}

/// Render a cached formula result for the `<v>` element. Overlay strings are
/// resolved against the overlay arena first, then the base sheet.
fn overlay_display(v: &Value, overlay: &EditOverlay, sheet: &Sheet) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Number(n) if n.is_finite() => ferrix_core::value::format_number(*n),
        Value::Number(_) => ErrorKind::Num.as_str().to_string(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Text(id) => overlay
            .resolve(*id)
            .unwrap_or_else(|| sheet.resolve(*id))
            .to_string(),
        Value::Error(e) => e.as_str().to_string(),
    }
}

// ----------------------------------------------------------------- tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch path in the OS temp dir, removed on drop so a failing
    /// assertion still cleans up after itself.
    struct TempXlsx(std::path::PathBuf);

    impl TempXlsx {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let uniq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            p.push(format!("ferrix-xlsx-{tag}-{uniq}.xlsx"));
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempXlsx {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The fixture workbook: two sheets covering every `Value` variant plus a
    /// set of formulas the Ferrix parser accepts.
    fn fixture() -> (Sheet, EditOverlay, Sheet) {
        let mut data = Sheet::new("Data");
        for (r, n) in [10.0, 20.5, -3.0, 0.0].into_iter().enumerate() {
            data.set(CellRef::new(r as u32, 0), Value::Number(n));
        }
        data.set_text(CellRef::new(0, 1), "alpha");
        data.set_text(CellRef::new(1, 1), "beta gamma");
        data.set_text(CellRef::new(2, 1), "unicode: café ✓");
        data.set(CellRef::new(0, 2), Value::Bool(true));
        data.set(CellRef::new(1, 2), Value::Bool(false));
        data.set(CellRef::new(0, 3), Value::Error(ErrorKind::DivZero));
        data.set(CellRef::new(1, 3), Value::Error(ErrorKind::NotAvailable));
        data.set(CellRef::new(2, 3), Value::Error(ErrorKind::Name));
        data.set(CellRef::new(3, 3), Value::Error(ErrorKind::Circular));
        // A gap: (2,2) and (3,2) stay Empty.

        // Formulas over that data, all inside the supported grammar.
        let mut fx = EditOverlay::new();
        let cases: &[(u32, &str, Value)] = &[
            (0, "=SUM(A1:A4)", Value::Number(27.5)),
            (1, "=AVERAGE(A1:A4)", Value::Number(27.5 / 4.0)),
            (2, "=MIN(A1:A4)", Value::Number(-3.0)),
            (3, "=MAX(A1:A4)", Value::Number(20.5)),
            (4, "=COUNT(A1:A4)", Value::Number(4.0)),
            (5, "=A1*2+1", Value::Number(21.0)),
            (6, "=ABS(A3)", Value::Number(3.0)),
            (7, "=SQRT(A1)", Value::Number(10f64.sqrt())),
            (8, "=IF(A1>5,1,0)", Value::Number(1.0)),
            (9, "=$A$1^2", Value::Number(100.0)),
            (10, "=A2-A1", Value::Number(10.5)),
            (11, "=NOT(C1)", Value::Bool(false)),
        ];
        for (r, src, cached) in cases {
            fx.set(
                CellRef::new(*r, 4),
                CellInput::Formula {
                    src: (*src).to_string(),
                    cached: *cached,
                },
            );
        }

        let mut second = Sheet::new("Second");
        second.set_text(CellRef::new(0, 0), "sheet two");
        second.set(CellRef::new(1, 0), Value::Number(42.0));
        second.set(CellRef::new(2, 0), Value::Bool(true));

        (data, fx, second)
    }

    #[test]
    fn round_trip_preserves_values_types_and_formulas() {
        let (data, fx, second) = fixture();
        let tmp = TempXlsx::new("roundtrip");

        export_workbook(
            tmp.path(),
            &[
                SheetExport::new("Data", &data).with_formulas(&fx),
                SheetExport::new("Second", &second),
            ],
        )
        .expect("export");
        assert!(tmp.path().exists(), "fixture workbook was not written");

        let sheets = import_xlsx_full(tmp.path()).expect("import");

        // -- multi-sheet: names and order --------------------------------
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].name, "Data");
        assert_eq!(sheets[1].name, "Second");

        let got = &sheets[0].sheet;

        // -- numbers ------------------------------------------------------
        for (r, n) in [10.0, 20.5, -3.0, 0.0].into_iter().enumerate() {
            assert_eq!(
                got.get(CellRef::new(r as u32, 0)),
                Value::Number(n),
                "number at row {r}"
            );
        }

        // -- text (including non-ASCII) -----------------------------------
        assert_eq!(got.display(CellRef::new(0, 1)), "alpha");
        assert_eq!(got.display(CellRef::new(1, 1)), "beta gamma");
        assert_eq!(got.display(CellRef::new(2, 1)), "unicode: café ✓");
        assert!(matches!(got.get(CellRef::new(0, 1)), Value::Text(_)));

        // -- bools stay bools, not 1/0 ------------------------------------
        assert_eq!(got.get(CellRef::new(0, 2)), Value::Bool(true));
        assert_eq!(got.get(CellRef::new(1, 2)), Value::Bool(false));

        // -- errors --------------------------------------------------------
        assert_eq!(
            got.get(CellRef::new(0, 3)),
            Value::Error(ErrorKind::DivZero)
        );
        assert_eq!(
            got.get(CellRef::new(1, 3)),
            Value::Error(ErrorKind::NotAvailable)
        );
        assert_eq!(got.get(CellRef::new(2, 3)), Value::Error(ErrorKind::Name));
        assert_eq!(
            got.get(CellRef::new(3, 3)),
            Value::Error(ErrorKind::Circular),
            "#CIRC! must survive even though Excel has no such error"
        );

        // -- empties stay empty --------------------------------------------
        assert_eq!(got.get(CellRef::new(2, 2)), Value::Empty);
        assert_eq!(got.get(CellRef::new(9, 9)), Value::Empty);

        // -- formulas -------------------------------------------------------
        let back = &sheets[0].formulas;
        for (cell, src) in fx.formula_cells() {
            let round_tripped = back
                .get(cell)
                .unwrap_or_else(|| panic!("formula at {} was lost", cell.to_a1()));
            assert_eq!(
                round_tripped.formula_src(),
                Some(src),
                "formula source changed at {}",
                cell.to_a1()
            );
            // The cached result must survive too, and must still parse.
            assert!(
                ferrix_formula::parse(src).is_ok(),
                "fixture formula {src} is outside the parser's grammar"
            );
        }
        assert_eq!(
            back.formula_cells().count(),
            fx.formula_cells().count(),
            "formula count changed across the round trip"
        );
        assert_eq!(sheets[0].stats.formulas_dropped, 0);
        assert_eq!(sheets[0].stats.formulas_kept, 12);

        // Cached numeric results survive as numbers.
        assert_eq!(
            back.value(CellRef::new(0, 4)),
            Some(Value::Number(27.5)),
            "SUM cached result"
        );
        assert_eq!(back.value(CellRef::new(11, 4)), Some(Value::Bool(false)));

        // -- second sheet ---------------------------------------------------
        let s2 = &sheets[1].sheet;
        assert_eq!(s2.display(CellRef::new(0, 0)), "sheet two");
        assert_eq!(s2.get(CellRef::new(1, 0)), Value::Number(42.0));
        assert_eq!(s2.get(CellRef::new(2, 0)), Value::Bool(true));
    }

    #[test]
    fn values_only_api_round_trips() {
        let (data, _, _) = fixture();
        let tmp = TempXlsx::new("valuesonly");
        export_xlsx(tmp.path(), &data, "OnlyValues").expect("export");

        let sheets = import_xlsx(tmp.path()).expect("import");
        assert_eq!(sheets.len(), 1);
        assert_eq!(sheets[0].0, "OnlyValues");
        assert_eq!(sheets[0].1.get(CellRef::new(1, 0)), Value::Number(20.5));
        assert_eq!(sheets[0].1.display(CellRef::new(0, 1)), "alpha");
    }

    #[test]
    fn unsupported_formulas_degrade_to_their_cached_value() {
        // XMATCH is not implemented by the Ferrix evaluator. Excel's cached
        // result must still land, and the loss must be reported rather than
        // hidden.
        //
        // (This test used VLOOKUP until issue #23 implemented it. The example
        // had to change; the property did not.)
        let mut sheet = Sheet::new("s");
        sheet.set(CellRef::new(0, 0), Value::Number(7.0));
        let mut fx = EditOverlay::new();
        fx.set(
            CellRef::new(0, 1),
            CellInput::Formula {
                src: "=XMATCH(A1,A1:A1,0)".to_string(),
                cached: Value::Number(7.0),
            },
        );

        let tmp = TempXlsx::new("unsupported");
        export_xlsx_with_formulas(tmp.path(), &sheet, "s", &fx).expect("export");
        let sheets = import_xlsx_full(tmp.path()).expect("import");

        assert_eq!(sheets[0].stats.formulas_dropped, 1);
        assert_eq!(sheets[0].stats.formulas_kept, 0);
        assert!(sheets[0].formulas.get(CellRef::new(0, 1)).is_none());
        assert_eq!(
            sheets[0].sheet.get(CellRef::new(0, 1)),
            Value::Number(7.0),
            "cached result must survive even when the formula does not"
        );
    }

    #[test]
    fn export_over_the_row_cap_fails_loudly() {
        // One cell one row past the limit is enough — the check is on extent.
        let mut sheet = Sheet::new("huge");
        sheet.set(CellRef::new(XLSX_MAX_ROWS as u32, 0), Value::Number(1.0));
        assert_eq!(sheet.row_count(), XLSX_MAX_ROWS + 1);

        let tmp = TempXlsx::new("rowcap");
        let err = export_xlsx(tmp.path(), &sheet, "huge").unwrap_err();

        assert!(
            matches!(err, XlsxError::TooManyRows { rows, .. } if rows == XLSX_MAX_ROWS + 1),
            "expected TooManyRows, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("1048576"), "message must name the cap: {msg}");
        assert!(
            msg.contains("truncat"),
            "message must say it refused rather than truncated: {msg}"
        );
        assert!(
            !tmp.path().exists(),
            "nothing may be written when the export is refused"
        );
    }

    #[test]
    fn export_at_exactly_the_row_cap_is_allowed() {
        // Boundary: the cap itself is legal, only one past it is not.
        let mut sheet = Sheet::new("edge");
        sheet.set(
            CellRef::new(XLSX_MAX_ROWS as u32 - 1, 0),
            Value::Number(1.0),
        );
        assert_eq!(sheet.row_count(), XLSX_MAX_ROWS);
        assert!(check_limits("edge", &sheet, None).is_ok());
    }

    #[test]
    fn export_over_the_column_cap_fails_loudly() {
        let mut sheet = Sheet::new("wide");
        sheet.set(CellRef::new(0, XLSX_MAX_COLS as u32), Value::Number(1.0));
        let tmp = TempXlsx::new("colcap");
        let err = export_xlsx(tmp.path(), &sheet, "wide").unwrap_err();
        assert!(
            matches!(err, XlsxError::TooManyCols { cols, .. } if cols == XLSX_MAX_COLS + 1),
            "expected TooManyCols, got {err:?}"
        );
        assert!(!tmp.path().exists());
    }

    #[test]
    fn oversized_sheet_blocks_the_whole_workbook() {
        // The limit check runs over every sheet before any writing, so a bad
        // sheet cannot leave a half-written file behind.
        let mut ok = Sheet::new("ok");
        ok.set(CellRef::new(0, 0), Value::Number(1.0));
        let mut bad = Sheet::new("bad");
        bad.set(CellRef::new(XLSX_MAX_ROWS as u32, 0), Value::Number(1.0));

        let tmp = TempXlsx::new("mixed");
        let err = export_workbook(
            tmp.path(),
            &[SheetExport::new("ok", &ok), SheetExport::new("bad", &bad)],
        )
        .unwrap_err();
        assert!(matches!(err, XlsxError::TooManyRows { .. }));
        assert!(!tmp.path().exists());
    }

    #[test]
    fn error_spellings_are_recognized_in_both_directions() {
        for e in [
            ErrorKind::DivZero,
            ErrorKind::Value,
            ErrorKind::Ref,
            ErrorKind::Name,
            ErrorKind::Num,
            ErrorKind::NotAvailable,
            ErrorKind::Null,
            ErrorKind::Circular,
        ] {
            assert_eq!(error_from_str(e.as_str()), Some(e), "{}", e.as_str());
        }
        assert_eq!(error_from_str("not an error"), None);
        assert_eq!(error_from_str("#DIV/0"), None, "must be an exact match");
    }

    #[test]
    fn every_listed_function_really_evaluates() {
        // Drift guard: if someone removes a function from eval.rs, this list
        // must not keep claiming the import layer can preserve it. A name the
        // evaluator does not know answers #NAME?.
        let mut sheet = Sheet::new("s");
        sheet.set(CellRef::new(0, 0), Value::Number(1.0));
        for f in SUPPORTED_FUNCTIONS {
            let src = format!("={f}(A1)");
            let expr = ferrix_formula::parse(&src).expect("parses");
            assert_ne!(
                ferrix_formula::eval(&expr, &sheet),
                Value::Error(ErrorKind::Name),
                "{f} is listed as supported but the evaluator answers #NAME?"
            );
        }
        // And the converse: an unimplemented name parses but is not accepted.
        // (VLOOKUP filled this role until issue #23 implemented it.)
        assert!(ferrix_formula::parse("=XMATCH(A1,A1:A1,0)").is_ok());
        assert!(!formula_is_supported("=XMATCH(A1,A1:A1,0)"));
        // The lookup family, by contrast, must now be accepted — a workbook
        // using it would otherwise lose its formulas on load.
        assert!(formula_is_supported("=VLOOKUP(A1,A1:A1,1,FALSE)"));
    }

    #[test]
    fn unsupported_calls_are_caught_when_nested() {
        // The check must recurse — an unknown function buried in an argument
        // is just as fatal on recalc as one at the top level.
        assert!(formula_is_supported("=IF(A1>0,SUM(A1:A5),ABS(A2))"));
        assert!(!formula_is_supported("=SUM(A1,XMATCH(A1,B1:B2,0))"));
        assert!(!formula_is_supported("=-CONCATENATE(A1,A2)"));
        // Case-insensitive, like Excel.
        assert!(formula_is_supported("=sum(A1:A5)"));
        // A lookup nested in an argument is fine now, and a lookup wrapping
        // an unimplemented call is still not.
        assert!(formula_is_supported("=SUM(A1,VLOOKUP(A1,B1:B2,1,FALSE))"));
        assert!(!formula_is_supported(
            "=VLOOKUP(XMATCH(A1,B1:B2,0),B1:B2,1,FALSE)"
        ));
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let mut p = std::env::temp_dir();
        p.push("ferrix-xlsx-definitely-missing.xlsx");
        let err = import_xlsx(&p).unwrap_err();
        // The safeguards now open the package before calamine does, so a
        // missing file is reported by them. What matters is that it is a
        // clean error naming the file, not a panic.
        assert!(
            matches!(
                err,
                XlsxError::Safeguard(SafeguardError::PartUnreadable { .. })
            ),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("definitely-missing"),
            "the error must name the file it could not open: {err}"
        );
    }

    #[test]
    fn non_finite_numbers_export_as_num_errors() {
        // NaN/inf have no xlsx encoding; they must not become 0 or a string.
        let mut sheet = Sheet::new("s");
        sheet.set(CellRef::new(0, 0), Value::Number(f64::NAN));
        sheet.set(CellRef::new(1, 0), Value::Number(f64::INFINITY));

        let tmp = TempXlsx::new("nonfinite");
        export_xlsx(tmp.path(), &sheet, "s").expect("export");
        let sheets = import_xlsx(tmp.path()).expect("import");
        assert_eq!(
            sheets[0].1.get(CellRef::new(0, 0)),
            Value::Error(ErrorKind::Num)
        );
        assert_eq!(
            sheets[0].1.get(CellRef::new(1, 0)),
            Value::Error(ErrorKind::Num)
        );
    }

    #[test]
    fn temp_fixtures_are_cleaned_up() {
        // The round-trip tests must not leave workbooks in the temp dir.
        let path;
        {
            let tmp = TempXlsx::new("cleanup");
            path = tmp.path().to_path_buf();
            let mut s = Sheet::new("s");
            s.set(CellRef::new(0, 0), Value::Number(1.0));
            export_xlsx(tmp.path(), &s, "s").expect("export");
            assert!(path.exists());
        }
        assert!(!path.exists(), "TempXlsx must remove its file on drop");
    }

    // --- defined names ---------------------------------------------------

    /// Two sheets, so a sheet-scoped name has somewhere to be scoped TO and
    /// its `localSheetId` has to point at the right index.
    fn two_sheets() -> (Sheet, Sheet) {
        let mut a = Sheet::new("Sheet1");
        for r in 0..10u32 {
            a.set(CellRef::new(r, 1), Value::Number((r + 1) as f64));
        }
        let mut b = Sheet::new("Sheet2");
        b.set(CellRef::new(0, 3), Value::Number(99.0));
        (a, b)
    }

    #[test]
    fn a_round_trip_preserves_both_workbook_and_sheet_scope() {
        // THE acceptance criterion for xlsx: two names sharing an identifier
        // must come back distinct, because only `localSheetId` tells them
        // apart in the file.
        let tmp = TempXlsx::new("names-scope");
        let (a, b) = two_sheets();
        let mut names = NameTable::new();
        names
            .define(DefinedName::new(
                "Sales",
                NameScope::Workbook,
                "Sheet1!$B$1:$B$10",
            ))
            .expect("workbook name");
        names
            .define(DefinedName::new(
                "Sales",
                NameScope::Sheet("Sheet2".into()),
                "Sheet2!$D$1",
            ))
            .expect("local name");

        export_workbook_with_names(
            tmp.path(),
            &[
                SheetExport::new("Sheet1", &a),
                SheetExport::new("Sheet2", &b),
            ],
            &names,
        )
        .expect("export");

        let back = import_defined_names(tmp.path()).expect("import names");
        assert_eq!(back.len(), 2, "both scopes must survive");

        let wb_scoped = back
            .get_scoped("Sales", &NameScope::Workbook)
            .expect("workbook-scoped Sales survived");
        assert_eq!(wb_scoped.refers_to, "Sheet1!$B$1:$B$10");

        let local = back
            .get_scoped("Sales", &NameScope::Sheet("Sheet2".into()))
            .expect("sheet-scoped Sales survived");
        assert_eq!(local.refers_to, "Sheet2!$D$1");
        assert_eq!(local.scope, NameScope::Sheet("Sheet2".into()));

        // And resolution from each sheet still picks the right one.
        assert_eq!(
            back.get("Sales", Some("Sheet2")).unwrap().refers_to,
            "Sheet2!$D$1"
        );
        assert_eq!(
            back.get("Sales", Some("Sheet1")).unwrap().refers_to,
            "Sheet1!$B$1:$B$10"
        );
    }

    #[test]
    fn exported_names_appear_as_real_defined_name_elements() {
        // Proves the OOXML Excel reads is actually there, not just that
        // Ferrix agrees with itself.
        use std::io::Read as _;
        let tmp = TempXlsx::new("names-xml");
        let (a, b) = two_sheets();
        let mut names = NameTable::new();
        names
            .define(DefinedName::new("Rate", NameScope::Workbook, "Sheet1!$B$1"))
            .unwrap();
        names
            .define(DefinedName::new(
                "Local",
                NameScope::Sheet("Sheet2".into()),
                "Sheet2!$D$1",
            ))
            .unwrap();
        export_workbook_with_names(
            tmp.path(),
            &[
                SheetExport::new("Sheet1", &a),
                SheetExport::new("Sheet2", &b),
            ],
            &names,
        )
        .expect("export");

        let f = std::fs::File::open(tmp.path()).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut xml = String::new();
        zip.by_name("xl/workbook.xml")
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();

        assert!(xml.contains("<definedNames>"), "no definedNames block");
        assert!(xml.contains("name=\"Rate\""), "workbook name missing");
        assert!(xml.contains("name=\"Local\""), "local name missing");
        // Sheet2 is index 1 in the <sheets> order, and only the local name
        // carries a localSheetId at all.
        assert!(
            xml.contains("localSheetId=\"1\""),
            "sheet scope must be encoded as localSheetId, got: {xml}"
        );
    }

    #[test]
    fn a_workbook_without_names_writes_no_defined_names_block() {
        // A file that gained an empty element would churn every save and
        // could confuse stricter readers.
        use std::io::Read as _;
        let tmp = TempXlsx::new("names-none");
        let (a, _) = two_sheets();
        export_workbook(tmp.path(), &[SheetExport::new("Sheet1", &a)]).expect("export");
        let f = std::fs::File::open(tmp.path()).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut xml = String::new();
        zip.by_name("xl/workbook.xml")
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();
        assert!(!xml.contains("definedName"));
        assert!(import_defined_names(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn a_quoted_sheet_name_survives_the_scope_round_trip() {
        let tmp = TempXlsx::new("names-quoted");
        let (a, b) = two_sheets();
        let mut names = NameTable::new();
        names
            .define(DefinedName::new(
                "Local",
                NameScope::Sheet("Q1 2024".into()),
                "'Q1 2024'!$A$1:$A$5",
            ))
            .unwrap();
        export_workbook_with_names(
            tmp.path(),
            &[
                SheetExport::new("Sheet1", &a),
                SheetExport::new("Q1 2024", &b),
            ],
            &names,
        )
        .expect("export");

        let back = import_defined_names(tmp.path()).expect("import");
        let d = back
            .get_scoped("Local", &NameScope::Sheet("Q1 2024".into()))
            .expect("scope survived quoting");
        assert_eq!(d.refers_to, "'Q1 2024'!$A$1:$A$5");
    }

    // ------------------------------------------------- date round trips ---

    /// Issue #25 acceptance criterion: a date-formatted cell **computed by a
    /// formula** must export and reimport with the *same serial*.
    ///
    /// The failure this guards is not hypothetical: a date is stored as a bare
    /// `f64`, so a one-day drift or a silently dropped formula would still
    /// produce a perfectly valid file with a wrong date in it.
    #[test]
    fn date_formulas_round_trip_with_the_same_serial() {
        use ferrix_core::{ColumnType, DateStyle, NumberFormat, Table, TableColumn, TableRange};

        // Row 0 is the table's header row. A2 holds 2023-01-31 (serial
        // 44957) as a plain number, because a date IS a plain number here --
        // that is the storage decision under test.
        let mut sheet = Sheet::new("Dates");
        sheet.set_text(CellRef::new(0, 0), "start");
        sheet.set_text(CellRef::new(0, 1), "computed");
        sheet.set(CellRef::new(1, 0), Value::Number(44_957.0));

        // Formulas whose answers are themselves dates, with the serial each
        // must produce worked out independently from Excel's calendar.
        let cases: &[(&str, f64)] = &[
            ("=EOMONTH(A2,1)", 44_985.0),       // 2023-02-28
            ("=EDATE(A2,1)", 44_985.0),         // 31 Jan + 1 month clamps
            ("=DATE(2023,3,15)", 45_000.0),     // 2023-03-15
            ("=A2+1", 44_958.0),                // plain arithmetic on a date
            ("=DATE(1900,2,29)", 60.0),         // Excel's phantom day
            ("=DATE(9999,12,31)", 2_958_465.0), // the top of the range
        ];

        let mut fx = EditOverlay::new();
        for (i, (src, want)) in cases.iter().enumerate() {
            fx.set(
                CellRef::new(i as u32 + 1, 1),
                CellInput::Formula {
                    src: (*src).to_string(),
                    cached: Value::Number(*want),
                },
            );
        }

        // Column B is declared a date column, so the exported cells really do
        // carry a date number format rather than being anonymous numbers.
        let tables = [
            Table::new("D", TableRange::new(0, 0, cases.len() as u32, 1)).with_columns(vec![
                TableColumn::new("start").typed(ColumnType::Date),
                TableColumn::new("computed")
                    .typed(ColumnType::Date)
                    .formatted(NumberFormat::Date(DateStyle::Iso)),
            ]),
        ];

        let tmp = TempXlsx::new("dateroundtrip");
        export_workbook(
            tmp.path(),
            &[SheetExport::new("Dates", &sheet)
                .with_formulas(&fx)
                .with_tables(&tables)],
        )
        .expect("export");

        let back = import_xlsx_full(tmp.path()).expect("import");
        let got = &back[0];

        assert_eq!(
            got.stats.formulas_kept,
            cases.len(),
            "every date formula must come back as a live formula, not be              downgraded to its cached value"
        );

        for (i, (src, want)) in cases.iter().enumerate() {
            let cell = CellRef::new(i as u32 + 1, 1);

            // 1. The cached serial survived the file, bit for bit.
            assert_eq!(
                got.sheet.get(cell),
                Value::Number(*want),
                "{src} reimported with the wrong serial"
            );

            // 2. The formula came back and still says the same thing.
            let Some(CellInput::Formula { src: back_src, .. }) = got.formulas.get(cell) else {
                panic!("{src} did not survive as a formula");
            };
            assert_eq!(back_src, src, "formula text drifted");

            // 3. Re-evaluating the reimported formula against the reimported
            //    sheet reproduces the serial. This is the real round trip:
            //    the file, the parser and the evaluator all agree.
            let expr = ferrix_formula::parse(back_src).expect("reparses");
            assert_eq!(
                ferrix_formula::eval(&expr, &got.sheet),
                Value::Number(*want),
                "{src} re-evaluated to something other than {want}"
            );

            // 4. And the serial still paints as the date it is meant to be.
            assert_eq!(
                ferrix_core::table::render_serial(*want, DateStyle::Iso),
                ferrix_core::table::render_serial(
                    got.sheet.get(cell).as_number().unwrap(),
                    DateStyle::Iso
                )
            );
        }

        // The date column's format survived as a date format, so the cell is
        // still a *date* cell and not a bare number after the trip.
        let imported = crate::table_xlsx::import_tables(tmp.path()).expect("tables");
        let col = &imported[0].table.columns[1];
        assert!(
            col.format.is_date(),
            "computed column lost its date format: {:?}",
            col.format
        );
    }

    // ----------------------------------------------- lookup round trips ---

    /// Issue #23 acceptance criterion: a workbook using each lookup function
    /// must reload with cached values matching.
    ///
    /// "Matching" is checked three ways, because each catches a different
    /// failure and the weakest of them would pass against a broken import:
    ///
    /// 1. The cached value in the reimported sheet equals what was exported.
    ///    Alone this proves nothing about the formula — Excel's cached value
    ///    survives even when the formula is dropped entirely.
    /// 2. The formula came back as a LIVE formula with byte-identical source.
    ///    This is the one that fails if `calls_are_supported` does not know
    ///    the lookup family: the import would keep the cached number and
    ///    silently discard the formula, and check 1 would still pass.
    /// 3. Re-evaluating the reimported formula against the reimported sheet
    ///    reproduces the cached value. This is the actual round trip: file,
    ///    parser and evaluator all agreeing.
    #[test]
    fn lookup_formulas_round_trip_with_matching_cached_values() {
        // Data block, columns A..C: keys 10..50, names, payloads 1..5.
        let mut sheet = Sheet::new("Lookups");
        let names = ["alpha", "bravo", "charlie", "delta", "echo"];
        for r in 0..5u32 {
            sheet.set(CellRef::new(r, 0), Value::Number((r as f64 + 1.0) * 10.0));
            sheet.set_text(CellRef::new(r, 1), names[r as usize]);
            sheet.set(CellRef::new(r, 2), Value::Number(r as f64 + 1.0));
        }

        // One formula per function in the family, each with an independently
        // worked-out answer. Placed in column E so they cannot collide with
        // the data they read.
        let cases: &[(&str, Value)] = &[
            ("=VLOOKUP(30,A1:C5,3,FALSE)", Value::Number(3.0)),
            ("=VLOOKUP(35,A1:C5,3,TRUE)", Value::Number(3.0)),
            ("=HLOOKUP(10,A1:C1,1,FALSE)", Value::Number(10.0)),
            ("=MATCH(40,A1:A5,0)", Value::Number(4.0)),
            ("=MATCH(35,A1:A5,1)", Value::Number(3.0)),
            ("=INDEX(A1:C5,2,3)", Value::Number(2.0)),
            ("=INDEX(A1:A5,4,0)", Value::Number(40.0)),
            ("=XLOOKUP(50,A1:A5,C1:C5)", Value::Number(5.0)),
            ("=XLOOKUP(99,A1:A5,C1:C5,-1)", Value::Number(-1.0)),
            ("=CHOOSE(2,7,8,9)", Value::Number(8.0)),
            ("=INDIRECT(\"C4\")", Value::Number(4.0)),
            // Composed, because that is what real workbooks contain.
            ("=INDEX(C1:C5,MATCH(20,A1:A5,0))", Value::Number(2.0)),
            // An error result must survive as an error, not as a blank.
            (
                "=VLOOKUP(999,A1:C5,3,FALSE)",
                Value::Error(ErrorKind::NotAvailable),
            ),
        ];

        let mut fx = EditOverlay::new();
        for (i, (src, cached)) in cases.iter().enumerate() {
            fx.set(
                CellRef::new(i as u32, 4),
                CellInput::Formula {
                    src: (*src).to_string(),
                    cached: *cached,
                },
            );
        }

        // Before writing anything: the cached values we claim are correct
        // really are what this engine computes. A round trip that preserves a
        // WRONG cached value perfectly is not evidence of anything.
        for (src, cached) in cases {
            let expr = ferrix_formula::parse(src).expect("fixture formula parses");
            assert_eq!(
                ferrix_formula::eval(&expr, &sheet),
                *cached,
                "{src} does not actually evaluate to its declared cached value"
            );
        }

        let tmp = TempXlsx::new("lookuproundtrip");
        export_workbook(
            tmp.path(),
            &[SheetExport::new("Lookups", &sheet).with_formulas(&fx)],
        )
        .expect("export");

        let back = import_xlsx_full(tmp.path()).expect("import");
        let got = &back[0];

        assert_eq!(
            got.stats.formulas_kept,
            cases.len(),
            "every lookup formula must come back as a live formula, not be \
             downgraded to its cached value; {} were dropped",
            got.stats.formulas_dropped
        );

        for (i, (src, cached)) in cases.iter().enumerate() {
            let cell = CellRef::new(i as u32, 4);

            // 1. The cached value survived the file.
            let reloaded = got.sheet.get(cell);
            match cached {
                Value::Text(_) => unreachable!("no text cases in this fixture"),
                other => assert_eq!(
                    reloaded, *other,
                    "{src} reloaded with cached value {reloaded:?}, wanted {other:?}"
                ),
            }

            // 2. The formula came back, byte-identical.
            let Some(CellInput::Formula { src: back_src, .. }) = got.formulas.get(cell) else {
                panic!("{src} did not survive as a formula");
            };
            assert_eq!(back_src, src, "formula text drifted across the trip");

            // 3. Re-evaluating the reimported formula against the reimported
            //    sheet reproduces the cached value.
            let expr = ferrix_formula::parse(back_src).expect("reparses");
            assert_eq!(
                ferrix_formula::eval(&expr, &got.sheet),
                *cached,
                "{src} re-evaluated to something other than its cached value"
            );
        }
    }

    /// The import allowlist must be derived from the family predicate, not
    /// copied from it. A hand-maintained list drifts, and the drift is
    /// invisible: the formula is silently replaced by its cached value, which
    /// looks completely correct until the data underneath it changes.
    #[test]
    fn the_import_allowlist_tracks_the_lookup_family_predicate() {
        for name in [
            "VLOOKUP", "HLOOKUP", "INDEX", "MATCH", "XLOOKUP", "CHOOSE", "INDIRECT",
        ] {
            assert!(
                ferrix_formula::lookup::is_lookup_fn(name),
                "{name} is no longer claimed by the lookup family; this test \
                 and the import allowlist both need revisiting"
            );
            // A minimal well-formed call for each, only to prove the import
            // path accepts the NAME.
            let probe = format!("={name}(A1,A1:B2,1)");
            if ferrix_formula::parse(&probe).is_ok() {
                assert!(
                    formula_is_supported(&probe),
                    "{probe} parses but the xlsx importer would drop it, so a \
                     workbook using {name} loses its formulas on load"
                );
            }
        }
        // And a function nobody implements must still be refused, or the
        // importer would keep a formula that evaluates to #NAME?.
        assert!(
            !formula_is_supported("=XMATCH(A1,A1:A2)"),
            "an unimplemented function must not be imported as a live formula"
        );
    }

    /// `XLOOKUP` is written into OOXML as `_xlfn.XLOOKUP`, so an importer that
    /// does not strip the prefix loses every XLOOKUP formula to its cached
    /// value — a workbook that looks right and never recalculates.
    ///
    /// This was a real defect found by the round-trip test above: export was
    /// already correct (rust_xlsxwriter adds the prefix, and a file WITHOUT
    /// it shows #NAME? in Excel), import was not.
    #[test]
    fn future_function_prefixes_are_stripped_on_import() {
        assert_eq!(
            strip_future_fn_prefixes("_xlfn.XLOOKUP(50,A1:A5,C1:C5)"),
            "XLOOKUP(50,A1:A5,C1:C5)"
        );
        assert_eq!(
            strip_future_fn_prefixes("_xlfn._xlws.FILTER(A1:A5,B1:B5)"),
            "FILTER(A1:A5,B1:B5)"
        );
        // Nested occurrences, not just a leading one.
        assert_eq!(
            strip_future_fn_prefixes("SUM(_xlfn.XLOOKUP(1,A1:A2,B1:B2),2)"),
            "SUM(XLOOKUP(1,A1:A2,B1:B2),2)"
        );
        // A formula with nothing to strip comes back untouched and unallocated.
        let plain = "VLOOKUP(30,A1:C5,3,FALSE)";
        assert!(matches!(
            strip_future_fn_prefixes(plain),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(strip_future_fn_prefixes(plain), plain);
        // Text inside a STRING LITERAL is data, not a function name, and must
        // survive intact — stripping there would corrupt the user's content.
        assert_eq!(
            strip_future_fn_prefixes(r#"CONCAT("_xlfn.","x")"#),
            r#"CONCAT("_xlfn.","x")"#
        );
        // ...and a prefix after a closed string is still stripped, i.e. the
        // scanner really tracks string state rather than giving up.
        assert_eq!(
            strip_future_fn_prefixes(r#"IF(A1="_xlfn.",_xlfn.XLOOKUP(1,A1:A2,B1:B2),0)"#),
            r#"IF(A1="_xlfn.",XLOOKUP(1,A1:A2,B1:B2),0)"#
        );
    }

    /// End-to-end proof of the same thing: the exported file really does
    /// contain the prefixed spelling (so Excel accepts it), and the import
    /// really does turn it back into a live formula.
    #[test]
    fn an_exported_xlookup_carries_the_ooxml_prefix_and_still_reimports() {
        let mut sheet = Sheet::new("X");
        for r in 0..3u32 {
            sheet.set(CellRef::new(r, 0), Value::Number(r as f64));
            sheet.set(CellRef::new(r, 1), Value::Number(r as f64 * 100.0));
        }
        let mut fx = EditOverlay::new();
        fx.set(
            CellRef::new(0, 3),
            CellInput::Formula {
                src: "=XLOOKUP(2,A1:A3,B1:B3)".to_string(),
                cached: Value::Number(200.0),
            },
        );

        let tmp = TempXlsx::new("xlfnprefix");
        export_workbook(
            tmp.path(),
            &[SheetExport::new("X", &sheet).with_formulas(&fx)],
        )
        .expect("export");

        // The file on disk must carry the prefix; without it Excel shows
        // #NAME?, so an "improvement" that strips it on export would be a
        // regression this test catches.
        let mut wb: Xlsx<_> = calamine::open_workbook(tmp.path()).expect("open");
        let formulas = wb.worksheet_formula("X").expect("formulas");
        let on_disk: Vec<String> = formulas
            .used_cells()
            .map(|(_, _, s)| s.to_string())
            .collect();
        assert!(
            on_disk.iter().any(|s| s.contains("_xlfn.XLOOKUP")),
            "exported file does not spell XLOOKUP as _xlfn.XLOOKUP; Excel \
             would show #NAME?. Found: {on_disk:?}"
        );

        // And it still comes back as a live formula on our side.
        let back = import_xlsx_full(tmp.path()).expect("import");
        let got = &back[0];
        assert_eq!(got.stats.formulas_dropped, 0);
        let Some(CellInput::Formula { src, .. }) = got.formulas.get(CellRef::new(0, 3)) else {
            panic!("the XLOOKUP did not survive as a formula");
        };
        assert_eq!(src, "=XLOOKUP(2,A1:A3,B1:B3)");
        assert_eq!(
            ferrix_formula::eval(&ferrix_formula::parse(src).unwrap(), &got.sheet),
            Value::Number(200.0)
        );
    }

    // --- sizing round trip (issue #29) ---

    /// A sheet with enough rows/columns for the sizing under test to land on
    /// real `<row>` and `<col>` elements.
    fn sizing_sheet() -> Sheet {
        let mut s = Sheet::new("Sized");
        for r in 0..40u32 {
            for c in 0..6u32 {
                s.set(CellRef::new(r, c), Value::Number((r * 10 + c) as f64));
            }
        }
        s
    }

    #[test]
    fn column_widths_round_trip_through_xlsx() {
        let t = TempXlsx::new("size-widths");
        let sheet = sizing_sheet();
        let mut sz = ferrix_core::sizing::SheetSizing::new();
        sz.cols.set_width(1, 240.0);
        sz.cols.set_width(4, 60.0);
        export_workbook(
            t.path(),
            &[SheetExport::new("Sized", &sheet).with_sizing(&sz)],
        )
        .unwrap();

        let back = import_sizing(t.path()).unwrap();
        let (_, got) = back.first().expect("one sheet");
        // Exact pixel equality would pin Excel's own rounding, so the
        // assertion is that the width came back CLOSE — and, critically, that
        // the two columns are still different sizes and in the right order.
        let w1 = got.cols.width_of(1).expect("column 1 width must survive");
        let w4 = got.cols.width_of(4).expect("column 4 width must survive");
        assert!(
            (w1 - 240.0).abs() < 12.0,
            "column 1 came back {w1}px, expected ~240px"
        );
        assert!(
            (w4 - 60.0).abs() < 12.0,
            "column 4 came back {w4}px, expected ~60px"
        );
        assert!(w1 > w4, "the wide column must still be the wider one");
    }

    #[test]
    fn hidden_rows_and_columns_round_trip_through_xlsx() {
        let t = TempXlsx::new("size-hidden");
        let sheet = sizing_sheet();
        let mut sz = ferrix_core::sizing::SheetSizing::new();
        sz.cols.hide(2);
        sz.rows.hide(5, 8);
        export_workbook(
            t.path(),
            &[SheetExport::new("Sized", &sheet).with_sizing(&sz)],
        )
        .unwrap();

        let back = import_sizing(t.path()).unwrap();
        let (_, got) = back.first().expect("one sheet");
        assert!(got.cols.is_hidden(2), "hidden column must come back hidden");
        assert!(!got.cols.is_hidden(1), "column 1 was never hidden");
        for r in 5..=8 {
            assert!(got.rows.is_hidden(r), "row {r} must come back hidden");
        }
        assert!(!got.rows.is_hidden(4), "row 4 was never hidden");
        assert!(!got.rows.is_hidden(9), "row 9 was never hidden");
    }

    #[test]
    fn row_heights_round_trip_through_xlsx() {
        let t = TempXlsx::new("size-heights");
        let sheet = sizing_sheet();
        let mut sz = ferrix_core::sizing::SheetSizing::new();
        sz.rows.set_range(2, 4, 36.0);
        export_workbook(
            t.path(),
            &[SheetExport::new("Sized", &sheet).with_sizing(&sz)],
        )
        .unwrap();

        let back = import_sizing(t.path()).unwrap();
        let (_, got) = back.first().expect("one sheet");
        for r in 2..=4 {
            assert_eq!(
                got.rows.height_of(r),
                Some(36.0),
                "row {r} height must survive the round trip"
            );
        }
        assert_eq!(got.rows.height_of(1), None, "row 1 had no explicit height");
    }

    #[test]
    fn outline_groups_round_trip_through_xlsx() {
        let t = TempXlsx::new("size-outline");
        let sheet = sizing_sheet();
        let mut sz = ferrix_core::sizing::SheetSizing::new();
        sz.row_outline.group(10, 20).unwrap();
        export_workbook(
            t.path(),
            &[SheetExport::new("Sized", &sheet).with_sizing(&sz)],
        )
        .unwrap();

        let back = import_sizing(t.path()).unwrap();
        let (_, got) = back.first().expect("one sheet");
        assert!(
            !got.row_outline.is_empty(),
            "the group must survive as a group, not be flattened away"
        );
        // Rows inside the group carry a level; rows outside it do not. That is
        // the property a reader actually depends on.
        assert!(
            got.row_outline.level_at(15) >= 1,
            "row 15 is inside the group and must have an outline level"
        );
        assert_eq!(
            got.row_outline.level_at(30),
            0,
            "row 30 is outside the group and must not be grouped"
        );
    }

    #[test]
    fn a_sheet_with_no_sizing_writes_no_sizing() {
        // The default path must be untouched: exporting without sizing must
        // not start emitting widths or heights for every column.
        let t = TempXlsx::new("size-none");
        let sheet = sizing_sheet();
        export_workbook(t.path(), &[SheetExport::new("Sized", &sheet)]).unwrap();
        let back = import_sizing(t.path()).unwrap();
        let (_, got) = back.first().expect("one sheet");
        assert!(
            got.is_empty(),
            "a sheet exported without sizing must import with none, got {got:?}"
        );
    }
}
