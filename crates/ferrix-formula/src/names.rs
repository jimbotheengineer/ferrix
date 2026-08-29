//! Named ranges — workbook-scoped and sheet-scoped defined names.
//!
//! A name is a *label for a reference*, nothing more. `Sales` standing for
//! `Sheet1!$B$2:$B$1000` means every formula that writes `SUM(Sales)` is
//! literally the formula `SUM(Sheet1!$B$2:$B$1000)`, resolved once at parse
//! time.
//!
//! ## Why resolution belongs in the parser
//!
//! The alternative — a new `Expr::Name(String)` variant carried through to
//! evaluation — would force every consumer of the tree to learn about names:
//! the dependency graph would need a second resolution pass to know what a
//! formula reads, the evaluator would need a name-aware `CellSource`, and the
//! columnar fast paths in [`crate::eval`] (which match on `Expr::Range` /
//! `Expr::XRange` to reach `sum_rect`) would silently stop firing.
//!
//! Resolving in the parser instead means a name *becomes* the range it stands
//! for before anything downstream sees it. `SUM(Sales)` over a 200M-row range
//! costs exactly what `SUM(Sheet1!$B$2:$B$200000000)` costs, because after
//! parsing they are the same tree. Nothing is materialised; the name table is
//! a handful of small strings beside the data.
//!
//! ## Scope
//!
//! A name is either workbook-scoped (visible from every sheet) or scoped to
//! one sheet. When both exist with the same identifier, the sheet-scoped one
//! wins *on its own sheet* and is invisible everywhere else — Excel's rule,
//! and the only one that makes a per-sheet `Total` useful.
//!
//! ## Renaming rewrites source text
//!
//! Renaming `Sales` to `Revenue` rewrites the SOURCE TEXT of every dependent
//! formula, through the same scanner [`crate::remap`] uses. That is safe here
//! in a way a sheet rename is not: a bare word in a formula that resolves to a
//! name has no other possible meaning, so replacing it cannot change what any
//! other part of the formula refers to.
//!
//! The rewrite is textual and never an AST round-trip, for the reason
//! [`crate::refscan`] documents: the parser discards the `$` markers the
//! tokenizer recorded, so re-rendering a parsed formula would quietly unpin
//! every absolute reference in the workbook.
//!
//! ## Deleting a name
//!
//! Deleting a name that formulas still use leaves those formulas' text
//! untouched and they become `#NAME?` — the parser no longer has an entry for
//! the word, which is exactly what `#NAME?` means. Rewriting them to `#REF!`
//! would lose the user's text and hide a recoverable mistake (redefining the
//! name puts everything back).

use crate::parser::{parse, quote_sheet_name, Expr, ParseError};
use crate::refscan;
use ferrix_core::{column_name, CellRef};

/// Excel's cap on the length of a defined name.
pub const MAX_NAME_LEN: usize = 255;

/// Where a name is visible.
#[derive(Clone, PartialEq, Eq, Debug, Hash, Default)]
pub enum NameScope {
    /// Visible from every sheet in the workbook.
    #[default]
    Workbook,
    /// Visible only from the named sheet, where it shadows any workbook-scoped
    /// name with the same identifier.
    ///
    /// Held by sheet NAME rather than `SheetId` because that is what both ends
    /// of the round trip speak: a formula writes `Sheet1!A1`, and OOXML's
    /// `<definedName>` addresses its scope through the sheet order in
    /// `xl/workbook.xml`. An id would have to be translated at both edges and
    /// would go stale the moment a sheet is renamed.
    Sheet(String),
}

impl NameScope {
    /// The sheet this scope is bound to, if any.
    pub fn sheet(&self) -> Option<&str> {
        match self {
            NameScope::Workbook => None,
            NameScope::Sheet(s) => Some(s.as_str()),
        }
    }

    /// Does a formula living on `home` see names in this scope?
    pub fn visible_from(&self, home: Option<&str>) -> bool {
        match self {
            NameScope::Workbook => true,
            NameScope::Sheet(s) => home.is_some_and(|h| h.eq_ignore_ascii_case(s)),
        }
    }

    /// Rename the sheet this scope points at, if it is the one that moved.
    pub fn rename_sheet(&mut self, old: &str, new: &str) {
        if let NameScope::Sheet(s) = self {
            if s.eq_ignore_ascii_case(old) {
                *s = new.to_string();
            }
        }
    }
}

/// One defined name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DefinedName {
    /// The identifier as the user typed it. Lookups are case-insensitive, but
    /// the original spelling is what the Name Manager and the xlsx file show.
    pub name: String,
    pub scope: NameScope,
    /// The formula text the name stands for, e.g. `Sheet1!$B$2:$B$1000`.
    ///
    /// Stored as text rather than a parsed range so that a name may also stand
    /// for a constant (`=0.96`) or a single cell, and so that the `$` markers
    /// survive the xlsx round trip verbatim.
    pub refers_to: String,
}

impl DefinedName {
    pub fn new(name: impl Into<String>, scope: NameScope, refers_to: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            scope,
            refers_to: refers_to.into(),
        }
    }

    /// The expression this name stands for.
    ///
    /// Parsed WITHOUT a name table, so a name can never expand into another
    /// name — that would make resolution recursive and a cycle of two names
    /// would hang the parser rather than report anything.
    pub fn target(&self) -> Result<Expr, ParseError> {
        parse(&self.refers_to)
    }

    /// Case-insensitive identity test against a bare identifier.
    pub fn is_named(&self, ident: &str) -> bool {
        self.name.eq_ignore_ascii_case(ident)
    }
}

/// Render a `refers_to` string for a rectangular selection on a sheet.
///
/// Absolute on both axes, matching what Excel writes: a name is a fixed
/// address, and a relative one would drift when a formula using it is filled.
pub fn refers_to_range(sheet: &str, start: CellRef, end: CellRef) -> String {
    let a1 = |c: CellRef| format!("${}${}", column_name(c.col), c.row + 1);
    if start == end {
        format!("{}!{}", quote_sheet_name(sheet), a1(start))
    } else {
        format!("{}!{}:{}", quote_sheet_name(sheet), a1(start), a1(end))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("a name cannot be empty")]
    Empty,
    #[error("a name may be at most {MAX_NAME_LEN} characters, not {0}")]
    TooLong(usize),
    #[error("a name must start with a letter or underscore, not {0:?}")]
    BadStart(char),
    #[error("{0:?} is not allowed in a name (use letters, digits, '_' or '.')")]
    BadChar(char),
    #[error("{0:?} looks like a cell reference, so it cannot be a name")]
    LooksLikeReference(String),
    #[error("{0:?} is reserved")]
    Reserved(String),
    #[error("{0:?} is already defined in this scope")]
    Duplicate(String),
    #[error("{0:?} is not defined in this scope")]
    Unknown(String),
    #[error("what {0:?} refers to is not a valid reference: {1}")]
    BadTarget(String, String),
}

/// Check an identifier against Excel's rules for a defined name.
///
/// The reference test is the load-bearing one: if `Tax1` were allowed as a
/// name, every formula containing it would be ambiguous with the cell at
/// column TAX row 1, and the tokenizer resolves that case as a reference. A
/// name that can never be seen is worse than a rejected one.
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(NameError::TooLong(name.chars().count()));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty");
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(NameError::BadStart(first));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            return Err(NameError::BadChar(c));
        }
    }
    if refscan::parse_ref(name).is_some() {
        return Err(NameError::LooksLikeReference(name.to_string()));
    }
    // R and C are Excel's R1C1 row/column markers; TRUE/FALSE are literals the
    // tokenizer resolves before it would ever consult the name table, so a
    // name spelled that way could never be reached.
    let upper = name.to_ascii_uppercase();
    if matches!(upper.as_str(), "R" | "C" | "TRUE" | "FALSE") {
        return Err(NameError::Reserved(name.to_string()));
    }
    Ok(())
}

/// Every defined name in a workbook.
///
/// A flat vector rather than a map: workbooks have tens of names, not
/// thousands, and iteration order matters to the Name Manager and to the
/// stability of an xlsx export.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameTable {
    entries: Vec<DefinedName>,
}

impl NameTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &DefinedName> {
        self.entries.iter()
    }

    /// Every name visible from `home`, sheet-scoped ones first.
    pub fn visible_from(&self, home: Option<&str>) -> Vec<&DefinedName> {
        let mut out: Vec<&DefinedName> = self
            .entries
            .iter()
            .filter(|d| d.scope.visible_from(home))
            .collect();
        out.sort_by_key(|d| matches!(d.scope, NameScope::Workbook));
        out
    }

    /// Look a name up exactly, in one scope.
    pub fn get_scoped(&self, ident: &str, scope: &NameScope) -> Option<&DefinedName> {
        self.entries
            .iter()
            .find(|d| d.is_named(ident) && scopes_equal(&d.scope, scope))
    }

    /// Look a name up as a formula on sheet `home` would see it.
    ///
    /// The sheet-scoped entry is tried first so a per-sheet name shadows the
    /// workbook one, which is what makes a name like `Total` usable on every
    /// sheet with a different meaning.
    pub fn get(&self, ident: &str, home: Option<&str>) -> Option<&DefinedName> {
        if let Some(h) = home {
            if let Some(d) = self.get_scoped(ident, &NameScope::Sheet(h.to_string())) {
                return Some(d);
            }
        }
        self.get_scoped(ident, &NameScope::Workbook)
    }

    /// Define a new name. Fails if the identifier is invalid or already taken
    /// in that scope.
    pub fn define(&mut self, entry: DefinedName) -> Result<(), NameError> {
        validate_name(&entry.name)?;
        entry
            .target()
            .map_err(|e| NameError::BadTarget(entry.refers_to.clone(), e.to_string()))?;
        if self.get_scoped(&entry.name, &entry.scope).is_some() {
            return Err(NameError::Duplicate(entry.name.clone()));
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Define or overwrite. Used by import, where the file is the truth.
    pub fn insert(&mut self, entry: DefinedName) -> Result<(), NameError> {
        validate_name(&entry.name)?;
        entry
            .target()
            .map_err(|e| NameError::BadTarget(entry.refers_to.clone(), e.to_string()))?;
        match self
            .entries
            .iter_mut()
            .find(|d| d.is_named(&entry.name) && scopes_equal(&d.scope, &entry.scope))
        {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
        Ok(())
    }

    /// Point an existing name at a different range.
    pub fn set_target(
        &mut self,
        ident: &str,
        scope: &NameScope,
        refers_to: &str,
    ) -> Result<(), NameError> {
        parse(refers_to).map_err(|e| NameError::BadTarget(refers_to.to_string(), e.to_string()))?;
        let slot = self
            .entries
            .iter_mut()
            .find(|d| d.is_named(ident) && scopes_equal(&d.scope, scope))
            .ok_or_else(|| NameError::Unknown(ident.to_string()))?;
        slot.refers_to = refers_to.to_string();
        Ok(())
    }

    /// Rename in place. The CALLER is responsible for rewriting dependent
    /// formula text — see [`rename_in_formula`] — because only the caller
    /// knows where the formulas are.
    pub fn rename(&mut self, ident: &str, scope: &NameScope, new: &str) -> Result<(), NameError> {
        validate_name(new)?;
        if self.get_scoped(ident, scope).is_none() {
            return Err(NameError::Unknown(ident.to_string()));
        }
        if !ident.eq_ignore_ascii_case(new) && self.get_scoped(new, scope).is_some() {
            return Err(NameError::Duplicate(new.to_string()));
        }
        let slot = self
            .entries
            .iter_mut()
            .find(|d| d.is_named(ident) && scopes_equal(&d.scope, scope))
            .expect("checked above");
        slot.name = new.to_string();
        Ok(())
    }

    pub fn remove(&mut self, ident: &str, scope: &NameScope) -> Option<DefinedName> {
        let i = self
            .entries
            .iter()
            .position(|d| d.is_named(ident) && scopes_equal(&d.scope, scope))?;
        Some(self.entries.remove(i))
    }

    /// Drop every name scoped to `sheet` — used when a sheet is deleted, so a
    /// local name cannot outlive the sheet it addressed.
    pub fn remove_sheet_scope(&mut self, sheet: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|d| {
            !d.scope
                .sheet()
                .is_some_and(|s| s.eq_ignore_ascii_case(sheet))
        });
        before - self.entries.len()
    }

    /// Follow a sheet rename through both the scope and every `refers_to`.
    pub fn rename_sheet(&mut self, old: &str, new: &str) {
        for d in &mut self.entries {
            d.scope.rename_sheet(old, new);
            d.refers_to = rewrite_sheet_qualifier(&d.refers_to, old, new);
        }
    }

    /// Resolve an identifier to the expression it stands for, as seen from
    /// `home`. This is what the parser calls.
    pub fn resolve(&self, ident: &str, home: Option<&str>) -> Option<Expr> {
        self.get(ident, home)?.target().ok()
    }
}

/// Scope equality, case-insensitive on the sheet name.
fn scopes_equal(a: &NameScope, b: &NameScope) -> bool {
    match (a, b) {
        (NameScope::Workbook, NameScope::Workbook) => true,
        (NameScope::Sheet(x), NameScope::Sheet(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// Replace the sheet qualifier in a `refers_to` string.
///
/// Deliberately narrow: `refers_to` is generated by Ferrix or read from a
/// file, and always has the shape `Sheet!$A$1[:$B$2]`. Anything that does not
/// match is returned untouched rather than guessed at.
fn rewrite_sheet_qualifier(refers_to: &str, old: &str, new: &str) -> String {
    let body = refers_to.strip_prefix('=').unwrap_or(refers_to);
    let Some(bang) = body.rfind('!') else {
        return refers_to.to_string();
    };
    let (qual, rest) = body.split_at(bang);
    let unquoted = qual
        .strip_prefix('\'')
        .and_then(|q| q.strip_suffix('\''))
        .map(|q| q.replace("''", "'"))
        .unwrap_or_else(|| qual.to_string());
    if !unquoted.eq_ignore_ascii_case(old) {
        return refers_to.to_string();
    }
    let lead = if refers_to.starts_with('=') { "=" } else { "" };
    format!("{lead}{}{rest}", quote_sheet_name(new))
}

// ------------------------------------------------------------ text rewriting

/// Does this formula's SOURCE TEXT mention `ident` as a bare name?
///
/// Uses the same scanner as [`crate::remap`], so text literals, quoted sheet
/// names, sheet qualifiers and function names are all excluded — `="Sales"` and
/// `=Sales!A1` do not reference the name `Sales`, and `=SALES()` is a call.
pub fn references_name(src: &str, ident: &str) -> bool {
    refscan::scan(src).iter().any(|w| {
        let word = &src[w.start..w.end];
        word.eq_ignore_ascii_case(ident) && refscan::parse_ref(word).is_none()
    })
}

/// Rewrite every bare occurrence of `old` in `src` to `new`.
///
/// Textual, not an AST round trip: re-rendering a parsed formula would drop
/// every `$` the user wrote (see [`crate::refscan`]), so a rename would
/// silently unpin absolute references across the whole workbook.
///
/// Words that parse as cell references are left alone, so a rename can never
/// turn `A1` into something else even if someone contrives a matching name.
pub fn rename_in_formula(src: &str, old: &str, new: &str) -> String {
    let words = refscan::scan(src);
    refscan::rewrite(src, &words, |_, w| {
        let word = &src[w.start..w.end];
        (word.eq_ignore_ascii_case(old) && refscan::parse_ref(word).is_none())
            .then(|| new.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cr(row: u32, col: u32) -> CellRef {
        CellRef::new(row, col)
    }

    fn sales() -> DefinedName {
        DefinedName::new("Sales", NameScope::Workbook, "Sheet1!$B$2:$B$1000")
    }

    #[test]
    fn refers_to_is_absolute_on_both_axes() {
        assert_eq!(
            refers_to_range("Sheet1", cr(1, 1), cr(999, 1)),
            "Sheet1!$B$2:$B$1000"
        );
        assert_eq!(refers_to_range("Sheet1", cr(0, 0), cr(0, 0)), "Sheet1!$A$1");
        // A sheet name needing quotes gets them.
        assert_eq!(
            refers_to_range("Q1 2024", cr(0, 0), cr(0, 0)),
            "'Q1 2024'!$A$1"
        );
    }

    #[test]
    fn a_name_resolves_to_the_range_expression_itself() {
        // The scale invariant: the resolved tree must be the SAME shape as the
        // explicit range, or the columnar SUM fast path stops firing and a
        // 200M-row name becomes a cell-by-cell walk.
        let mut t = NameTable::new();
        t.define(sales()).unwrap();
        let resolved = t.resolve("Sales", None).unwrap();
        let explicit = parse("=Sheet1!$B$2:$B$1000").unwrap();
        assert_eq!(resolved, explicit);
        assert!(
            matches!(resolved, Expr::XRange(_, _, _)),
            "must stay a range, not become a materialised list"
        );
    }

    #[test]
    fn lookup_is_case_insensitive_but_spelling_is_kept() {
        let mut t = NameTable::new();
        t.define(sales()).unwrap();
        assert!(t.get("SALES", None).is_some());
        assert!(t.get("sales", None).is_some());
        assert_eq!(t.get("sAlEs", None).unwrap().name, "Sales");
    }

    #[test]
    fn sheet_scope_shadows_workbook_scope_only_on_its_own_sheet() {
        let mut t = NameTable::new();
        t.define(sales()).unwrap();
        t.define(DefinedName::new(
            "Sales",
            NameScope::Sheet("Sheet2".into()),
            "Sheet2!$D$1:$D$5",
        ))
        .unwrap();

        assert_eq!(
            t.get("Sales", Some("Sheet2")).unwrap().refers_to,
            "Sheet2!$D$1:$D$5",
            "sheet-scoped name must win on its own sheet"
        );
        assert_eq!(
            t.get("Sales", Some("Sheet1")).unwrap().refers_to,
            "Sheet1!$B$2:$B$1000",
            "the local name must be invisible from another sheet"
        );
        assert_eq!(
            t.get("Sales", None).unwrap().refers_to,
            "Sheet1!$B$2:$B$1000"
        );
    }

    #[test]
    fn duplicate_in_the_same_scope_is_refused_but_a_different_scope_is_not() {
        let mut t = NameTable::new();
        t.define(sales()).unwrap();
        assert_eq!(
            t.define(sales()).unwrap_err(),
            NameError::Duplicate("Sales".into())
        );
        // Same identifier, different scope: fine.
        t.define(DefinedName::new(
            "Sales",
            NameScope::Sheet("Sheet2".into()),
            "Sheet2!$A$1",
        ))
        .unwrap();
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn names_that_look_like_cell_references_are_refused() {
        // Tax1 = column TAX, row 1. The tokenizer resolves it as a reference,
        // so a name spelled that way could never be reached.
        assert_eq!(
            validate_name("Tax1"),
            Err(NameError::LooksLikeReference("Tax1".into()))
        );
        assert_eq!(
            validate_name("A1"),
            Err(NameError::LooksLikeReference("A1".into()))
        );
        // Four letters is wider than XFD, so it is unambiguously a name.
        validate_name("Data1").unwrap();
    }

    #[test]
    fn name_syntax_rules() {
        assert_eq!(validate_name(""), Err(NameError::Empty));
        assert_eq!(validate_name("1Sales"), Err(NameError::BadStart('1')));
        assert_eq!(validate_name("My Name"), Err(NameError::BadChar(' ')));
        assert_eq!(validate_name("R"), Err(NameError::Reserved("R".into())));
        assert_eq!(
            validate_name("TRUE"),
            Err(NameError::Reserved("TRUE".into()))
        );
        assert_eq!(
            validate_name(&"a".repeat(MAX_NAME_LEN + 1)),
            Err(NameError::TooLong(MAX_NAME_LEN + 1))
        );
        validate_name("_hidden").unwrap();
        validate_name("Q1.Sales").unwrap();
    }

    #[test]
    fn a_target_that_is_not_a_reference_is_refused() {
        let mut t = NameTable::new();
        let err = t
            .define(DefinedName::new("Bad", NameScope::Workbook, "Sheet1!!!"))
            .unwrap_err();
        assert!(matches!(err, NameError::BadTarget(_, _)));
        assert!(t.is_empty(), "a rejected name must not land in the table");
    }

    #[test]
    fn rename_rewrites_dependent_formula_text() {
        assert_eq!(
            rename_in_formula("=SUM(Sales)*2", "Sales", "Revenue"),
            "=SUM(Revenue)*2"
        );
        // Case-insensitive match, canonical replacement.
        assert_eq!(
            rename_in_formula("=sales+1", "Sales", "Revenue"),
            "=Revenue+1"
        );
        // Every occurrence.
        assert_eq!(
            rename_in_formula("=Sales+Sales", "Sales", "Revenue"),
            "=Revenue+Revenue"
        );
    }

    #[test]
    fn rename_leaves_dollars_and_spacing_alone() {
        // The whole reason the rewrite is textual: an AST round trip would
        // drop every `$` in the formula.
        let src = "= SUM($A$1:$A$9) + Sales ";
        assert_eq!(
            rename_in_formula(src, "Sales", "Revenue"),
            "= SUM($A$1:$A$9) + Revenue "
        );
    }

    #[test]
    fn rename_does_not_touch_text_literals_sheet_names_or_calls() {
        // "Sales" the string is not the name Sales.
        assert_eq!(
            rename_in_formula("=\"Sales\"&Sales", "Sales", "Revenue"),
            "=\"Sales\"&Revenue"
        );
        // Sales!A1 is a SHEET called Sales.
        assert_eq!(
            rename_in_formula("=Sales!A1", "Sales", "Revenue"),
            "=Sales!A1"
        );
        // SALES( is a function call, not a name.
        assert_eq!(
            rename_in_formula("=SALES(1)", "Sales", "Revenue"),
            "=SALES(1)"
        );
    }

    #[test]
    fn references_name_agrees_with_the_rewriter() {
        assert!(references_name("=SUM(Sales)", "Sales"));
        assert!(references_name("=sales*2", "SALES"));
        assert!(!references_name("=\"Sales\"", "Sales"));
        assert!(!references_name("=Sales!A1", "Sales"));
        assert!(!references_name("=SALES(1)", "Sales"));
        assert!(!references_name("=SUM(A1:A9)", "Sales"));
    }

    #[test]
    fn removing_a_sheet_drops_only_its_local_names() {
        let mut t = NameTable::new();
        t.define(sales()).unwrap();
        t.define(DefinedName::new(
            "Local",
            NameScope::Sheet("Sheet2".into()),
            "Sheet2!$A$1",
        ))
        .unwrap();
        assert_eq!(t.remove_sheet_scope("SHEET2"), 1);
        assert_eq!(t.len(), 1);
        assert!(t.get("Sales", None).is_some());
    }

    #[test]
    fn renaming_a_sheet_follows_both_the_scope_and_the_target() {
        let mut t = NameTable::new();
        t.define(DefinedName::new(
            "Local",
            NameScope::Sheet("Sheet2".into()),
            "Sheet2!$A$1:$A$9",
        ))
        .unwrap();
        t.rename_sheet("Sheet2", "Q1 2024");
        let d = t.get("Local", Some("Q1 2024")).unwrap();
        assert_eq!(d.scope, NameScope::Sheet("Q1 2024".into()));
        assert_eq!(d.refers_to, "'Q1 2024'!$A$1:$A$9");
        // And it still parses back to the same rectangle.
        assert!(matches!(d.target().unwrap(), Expr::XRange(s, _, _) if s == "Q1 2024"));
    }

    #[test]
    fn rename_and_remove_report_unknown_names() {
        let mut t = NameTable::new();
        assert_eq!(
            t.rename("Nope", &NameScope::Workbook, "Other"),
            Err(NameError::Unknown("Nope".into()))
        );
        assert!(t.remove("Nope", &NameScope::Workbook).is_none());
    }

    #[test]
    fn rename_refuses_to_collide_but_allows_a_case_change() {
        let mut t = NameTable::new();
        t.define(sales()).unwrap();
        t.define(DefinedName::new(
            "Costs",
            NameScope::Workbook,
            "Sheet1!$C$1",
        ))
        .unwrap();
        assert_eq!(
            t.rename("Sales", &NameScope::Workbook, "Costs"),
            Err(NameError::Duplicate("Costs".into()))
        );
        t.rename("Sales", &NameScope::Workbook, "SALES").unwrap();
        assert_eq!(t.get("sales", None).unwrap().name, "SALES");
    }

    #[test]
    fn visible_from_lists_sheet_scope_first() {
        let mut t = NameTable::new();
        t.define(sales()).unwrap();
        t.define(DefinedName::new(
            "Local",
            NameScope::Sheet("Sheet2".into()),
            "Sheet2!$A$1",
        ))
        .unwrap();
        let from_two: Vec<&str> = t
            .visible_from(Some("Sheet2"))
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(from_two, vec!["Local", "Sales"]);
        let from_one: Vec<&str> = t
            .visible_from(Some("Sheet1"))
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(
            from_one,
            vec!["Sales"],
            "Sheet2's local name is not visible"
        );
    }

    #[test]
    fn set_target_repoints_an_existing_name() {
        let mut t = NameTable::new();
        t.define(sales()).unwrap();
        t.set_target("Sales", &NameScope::Workbook, "Sheet1!$C$1:$C$9")
            .unwrap();
        assert_eq!(t.get("Sales", None).unwrap().refers_to, "Sheet1!$C$1:$C$9");
        assert!(matches!(
            t.set_target("Sales", &NameScope::Workbook, "))"),
            Err(NameError::BadTarget(_, _))
        ));
    }
}
