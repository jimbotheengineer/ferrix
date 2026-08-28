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
use ferrix_core::{CellInput, CellRef, EditOverlay, ErrorKind, Sheet, Value};
use ferrix_formula::Expr;
use rust_xlsxwriter::{Formula, Workbook};

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
            SUPPORTED_FUNCTIONS.contains(&name.to_ascii_uppercase().as_str())
                && args.iter().all(calls_are_supported)
        }
        Expr::Unary(_, a) => calls_are_supported(a),
        Expr::Binary(_, a, b) => calls_are_supported(a) && calls_are_supported(b),
        Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Ref(_) | Expr::Range(_, _) => true,
    }
}

/// Can Ferrix keep this formula source as a live formula?
fn formula_is_supported(src: &str) -> bool {
    ferrix_formula::parse(src).is_ok_and(|e| calls_are_supported(&e))
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
    let path = path.as_ref();
    let disp = path.display().to_string();
    let read_err = |e: calamine::XlsxError| XlsxError::Read {
        path: disp.clone(),
        source: Box::new(e),
    };

    let mut wb: Xlsx<_> = calamine::open_workbook(path).map_err(read_err)?;
    let names = wb.sheet_names();
    if names.is_empty() {
        return Err(XlsxError::NoSheets);
    }

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        // One sheet resident at a time — peak memory is the largest sheet,
        // not the whole workbook.
        let range = wb.worksheet_range(&name).map_err(read_err)?;
        // Formulas are a separate pass; a workbook with none costs nothing.
        let formulas = wb.worksheet_formula(&name).map_err(read_err)?;
        out.push(build_sheet(&name, &range, &formulas));
    }
    Ok(out)
}

fn build_sheet(
    name: &str,
    range: &calamine::Range<Data>,
    formulas: &calamine::Range<String>,
) -> ImportedSheet {
    let mut sheet = Sheet::new(name);
    let mut overlay = EditOverlay::new();
    let mut stats = ImportStats::default();

    // `used_cells` yields positions relative to the range's own origin.
    let (r0, c0) = range.start().unwrap_or((0, 0));
    for (r, c, data) in range.used_cells() {
        let cell = CellRef::new(r0 + r as u32, c0 + c as u32);
        let value = data_to_value(data, &mut sheet);
        if value.is_empty() {
            continue;
        }
        stats.cells += 1;
        sheet.set(cell, value);
    }

    let (fr0, fc0) = formulas.start().unwrap_or((0, 0));
    for (r, c, src) in formulas.used_cells() {
        if src.is_empty() {
            continue;
        }
        let cell = CellRef::new(fr0 + r as u32, fc0 + c as u32);
        // Our own error-constant encoding (`=#DIV/0!`) is not a real formula.
        // The cached value already decoded it; do not count it as a loss.
        if error_from_str(src).is_some() {
            continue;
        }
        // xlsx stores the body without the leading '='.
        let src = format!("={src}");
        if !formula_is_supported(&src) {
            // An Excel function Ferrix does not implement, a cross-sheet
            // reference, or a structured table reference. Keep the cached
            // value, drop the formula.
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

    ImportedSheet {
        name: name.to_string(),
        sheet,
        formulas: overlay,
        stats,
    }
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

// ---------------------------------------------------------------- export ---

/// One worksheet to write.
pub struct SheetExport<'a> {
    pub name: &'a str,
    pub sheet: &'a Sheet,
    /// Formulas to write instead of the sheet's cached values.
    pub formulas: Option<&'a EditOverlay>,
}

impl<'a> SheetExport<'a> {
    pub fn new(name: &'a str, sheet: &'a Sheet) -> Self {
        Self {
            name,
            sheet,
            formulas: None,
        }
    }

    pub fn with_formulas(mut self, overlay: &'a EditOverlay) -> Self {
        self.formulas = Some(overlay);
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
        let ws = wb.add_worksheet_with_constant_memory();
        ws.set_name(s.name).map_err(write_err)?;
        write_sheet(ws, s).map_err(write_err)?;
    }
    wb.save(path).map_err(write_err)?;
    Ok(())
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
    Ok(())
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
        // VLOOKUP is not in the Ferrix grammar. Excel's cached result must
        // still land, and the loss must be reported rather than hidden.
        let mut sheet = Sheet::new("s");
        sheet.set(CellRef::new(0, 0), Value::Number(7.0));
        let mut fx = EditOverlay::new();
        fx.set(
            CellRef::new(0, 1),
            CellInput::Formula {
                src: "=VLOOKUP(A1,A1:A1,1,FALSE)".to_string(),
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
        assert!(ferrix_formula::parse("=VLOOKUP(A1,A1:A1,1)").is_ok());
        assert!(!formula_is_supported("=VLOOKUP(A1,A1:A1,1)"));
    }

    #[test]
    fn unsupported_calls_are_caught_when_nested() {
        // The check must recurse — an unknown function buried in an argument
        // is just as fatal on recalc as one at the top level.
        assert!(formula_is_supported("=IF(A1>0,SUM(A1:A5),ABS(A2))"));
        assert!(!formula_is_supported("=SUM(A1,VLOOKUP(A1,B1:B2,1))"));
        assert!(!formula_is_supported("=-CONCATENATE(A1,A2)"));
        // Case-insensitive, like Excel.
        assert!(formula_is_supported("=sum(A1:A5)"));
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let mut p = std::env::temp_dir();
        p.push("ferrix-xlsx-definitely-missing.xlsx");
        let err = import_xlsx(&p).unwrap_err();
        assert!(matches!(err, XlsxError::Read { .. }), "got {err:?}");
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
}
