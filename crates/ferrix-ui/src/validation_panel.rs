//! The data-validation editor and the in-cell autocomplete state (issue #41).
//!
//! ## What lives here and what does not
//!
//! Modelled on [`crate::cond_format`], for the same reason: the model beneath
//! (`ferrix_core::validate`) and the xlsx round trip (`ferrix_io::validate_xlsx`)
//! are complete on their own, and what was missing was any way for a user to
//! *make* a rule. This module is that way. It owns no checking and no storage
//! — every mutation is reported back through [`ValidationOutcome`] and applied
//! by `app.rs`, so the dialog can borrow the store while listing it.
//!
//! ## Scope, and why a rule is never per cell
//!
//! A rule is attached to a RANGE. `apply` writes exactly one
//! [`RangeValidation`] however many cells the user had selected, so a rule
//! over a 200M-row column is one small entry —
//! `validation_over_a_huge_selection_stores_one_entry` asserts it.
//!
//! ## Autocomplete, and why Escape lives in the UI
//!
//! [`AutocompleteState`] holds whether the popup is showing. The MODEL
//! (`ferrix_core::autocomplete`) deliberately has no "open" flag: dismissal is
//! a UI concern, and Escape must clear the popup WITHOUT touching the edit
//! buffer. Keeping the flag here is what makes
//! `escape_dismisses_the_popup_without_altering_the_typed_text` possible to
//! satisfy — the dismissal path has no access to the buffer at all.

use egui::RichText;
use ferrix_core::validate::{ErrorStyle, RangeValidation, SheetValidation, ValueDomain};
use ferrix_core::{CellRef, CmpOp, Suggestions, TableRange, ValidationRule};

use crate::theme::Theme;

// ============================================================ autocomplete ==

/// The suggestion popup's live state.
///
/// Bounded by construction: [`Suggestions`] is capped at
/// `ferrix_core::autocomplete::MAX_SUGGESTIONS` items, so this struct's
/// footprint has a hard ceiling regardless of the column behind it.
#[derive(Clone, Debug, Default)]
pub struct AutocompleteState {
    /// The cell the popup belongs to. `None` when nothing is showing.
    pub cell: Option<CellRef>,
    pub suggestions: Suggestions,
    /// Highlighted row, for ↑/↓ then Tab/Enter.
    pub highlight: usize,
    /// Set by Escape. While true, no popup is built for this edit however
    /// much more the user types — otherwise the next keystroke would pop it
    /// straight back up and Escape would look broken.
    pub dismissed: bool,
    /// True when the popup is a validation LIST dropdown rather than a
    /// column-scan suggestion. Drawn the same; sourced differently.
    pub from_list: bool,
}

impl AutocompleteState {
    /// Is a popup on screen right now?
    pub fn is_open(&self) -> bool {
        self.cell.is_some() && !self.suggestions.is_empty()
    }

    /// The highlighted suggestion, if any.
    pub fn current(&self) -> Option<&str> {
        self.suggestions
            .items
            .get(self.highlight)
            .map(String::as_str)
    }

    /// Escape. Closes the popup and NOTHING else.
    ///
    /// Takes `&mut self` and no other argument on purpose: it is structurally
    /// incapable of touching the edit buffer, which is the property the
    /// acceptance criterion names.
    pub fn dismiss(&mut self) {
        self.cell = None;
        self.suggestions = Suggestions::default();
        self.highlight = 0;
        self.dismissed = true;
    }

    /// A new edit began. Clears the dismissal so the next cell can suggest.
    pub fn reset(&mut self) {
        self.cell = None;
        self.suggestions = Suggestions::default();
        self.highlight = 0;
        self.dismissed = false;
        self.from_list = false;
    }

    /// Install a fresh set of suggestions for `cell`.
    pub fn offer(&mut self, cell: CellRef, s: Suggestions, from_list: bool) {
        if self.dismissed {
            return;
        }
        // Keep the highlight in range when the list shrinks under it.
        if self.highlight >= s.items.len() {
            self.highlight = 0;
        }
        self.cell = (!s.is_empty()).then_some(cell);
        self.suggestions = s;
        self.from_list = from_list;
    }

    pub fn move_highlight(&mut self, delta: isize) {
        let n = self.suggestions.items.len();
        if n == 0 {
            return;
        }
        let i = self.highlight as isize + delta;
        self.highlight = i.rem_euclid(n as isize) as usize;
    }
}

/// Draw the suggestion popup just under `anchor`, returning a click.
///
/// Returns the accepted value when the user clicks one. Keyboard acceptance is
/// handled by the app, which owns the edit buffer.
pub fn show_autocomplete(
    ctx: &egui::Context,
    st: &AutocompleteState,
    anchor: egui::Rect,
    th: Theme,
) -> Option<String> {
    if !st.is_open() {
        return None;
    }
    let mut picked = None;
    egui::Area::new(egui::Id::new("ferrix_autocomplete"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(anchor.min.x, anchor.max.y + 1.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(anchor.width().max(140.0));
                for (i, item) in st.suggestions.items.iter().enumerate() {
                    let text = if i == st.highlight {
                        RichText::new(item).color(th.accent).strong()
                    } else {
                        RichText::new(item).color(th.text)
                    };
                    if ui.selectable_label(i == st.highlight, text).clicked() {
                        picked = Some(item.clone());
                    }
                }
                if st.suggestions.truncated {
                    ui.label(
                        RichText::new("… type more to narrow")
                            .color(th.text_dim)
                            .small(),
                    );
                }
            });
        });
    picked
}

// ==================================================================== form ==

/// The dialog's live field values.
///
/// Every domain's fields are held at once rather than in a per-domain enum, so
/// flipping the domain selector back and forth does not discard what the user
/// typed — the same choice `cond_format::RuleForm` makes and for the same
/// reason.
#[derive(Clone, PartialEq, Debug)]
pub struct ValidationForm {
    pub domain: ValueDomain,
    /// Which comparison shape the numeric domains use.
    pub compare: CompareKind,
    pub op: CmpOp,
    /// Bounds as TYPED. Parsed on commit, so a half-entered "-" or "1e" does
    /// not snap the field back to zero mid-keystroke.
    pub min_text: String,
    pub max_text: String,
    /// One value per line — the way every spreadsheet asks for a list.
    pub list_text: String,
    pub formula: String,
    pub message: String,
    pub title: String,
    pub style: ErrorStyle,
    pub allow_empty: bool,
    pub show_dropdown: bool,
}

/// How a numeric domain's bounds are shaped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompareKind {
    Between,
    NotBetween,
    /// A single comparison against `min_text`.
    Compare,
    /// No bound at all — the domain check alone.
    Any,
}

impl CompareKind {
    pub const ALL: [CompareKind; 4] = [
        CompareKind::Between,
        CompareKind::NotBetween,
        CompareKind::Compare,
        CompareKind::Any,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CompareKind::Between => "between",
            CompareKind::NotBetween => "not between",
            CompareKind::Compare => "compared to",
            CompareKind::Any => "any value of this type",
        }
    }
}

impl Default for ValidationForm {
    fn default() -> Self {
        Self {
            domain: ValueDomain::List,
            compare: CompareKind::Between,
            op: CmpOp::Ge,
            min_text: "0".into(),
            max_text: "100".into(),
            list_text: String::new(),
            formula: String::new(),
            message: String::new(),
            title: String::new(),
            style: ErrorStyle::Stop,
            allow_empty: true,
            show_dropdown: true,
        }
    }
}

impl ValidationForm {
    /// Seed from an existing rule, so Edit opens on what is there.
    pub fn from_rule(r: &RangeValidation) -> Self {
        let mut f = ValidationForm {
            domain: r.domain,
            message: r.message.clone().unwrap_or_default(),
            title: r.title.clone().unwrap_or_default(),
            style: r.style,
            allow_empty: r.allow_empty,
            show_dropdown: r.show_dropdown,
            ..Default::default()
        };
        match &r.rule {
            ValidationRule::Between { min, max } => {
                f.compare = CompareKind::Between;
                f.min_text = ferrix_core::format_number(*min);
                f.max_text = ferrix_core::format_number(*max);
            }
            ValidationRule::NotBetween { min, max } => {
                f.compare = CompareKind::NotBetween;
                f.min_text = ferrix_core::format_number(*min);
                f.max_text = ferrix_core::format_number(*max);
            }
            ValidationRule::Compare { op, value } => {
                f.compare = CompareKind::Compare;
                f.op = *op;
                f.min_text = ferrix_core::format_number(*value);
            }
            ValidationRule::TextLength { min, max } => {
                f.compare = CompareKind::Between;
                f.min_text = min.to_string();
                f.max_text = max.to_string();
            }
            ValidationRule::OneOf(v) => f.list_text = v.join("\n"),
            ValidationRule::CustomFormula(s) => f.formula.clone_from(s),
            ValidationRule::Regex(s) => f.formula.clone_from(s),
            ValidationRule::None | ValidationRule::Unique => f.compare = CompareKind::Any,
        }
        f
    }

    fn min(&self) -> f64 {
        self.min_text.trim().parse().unwrap_or(0.0)
    }

    fn max(&self) -> f64 {
        self.max_text.trim().parse().unwrap_or(0.0)
    }

    /// The list values, split on newlines AND commas, blanks dropped.
    ///
    /// Both separators are accepted because both are natural: the field is
    /// laid out one-per-line, but comma-separated is the spreadsheet
    /// convention (it is how Excel's inline list is written), and a user who
    /// types `North, South, East` on one line means three values, not one.
    /// Splitting on newlines alone turned that whole line into a SINGLE allowed
    /// value, so the dropdown offered one giant option and every real cell
    /// failed validation.
    pub fn list_values(&self) -> Vec<String> {
        self.list_text
            .lines()
            .flat_map(|line| line.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The predicate this form describes.
    pub fn to_rule(&self) -> ValidationRule {
        match self.domain {
            ValueDomain::List => ValidationRule::OneOf(self.list_values()),
            ValueDomain::Custom => ValidationRule::CustomFormula(self.formula.clone()),
            ValueDomain::TextLength => match self.compare {
                CompareKind::Any => ValidationRule::None,
                _ => ValidationRule::TextLength {
                    min: self.min().max(0.0) as u32,
                    max: self.max().max(0.0) as u32,
                },
            },
            ValueDomain::Any => ValidationRule::None,
            // The numeric domains.
            _ => match self.compare {
                CompareKind::Between => ValidationRule::Between {
                    min: self.min(),
                    max: self.max(),
                },
                CompareKind::NotBetween => ValidationRule::NotBetween {
                    min: self.min(),
                    max: self.max(),
                },
                CompareKind::Compare => ValidationRule::Compare {
                    op: self.op,
                    value: self.min(),
                },
                CompareKind::Any => ValidationRule::None,
            },
        }
    }

    /// The complete rule over `range`.
    pub fn to_validation(&self, range: TableRange) -> RangeValidation {
        let mut r = RangeValidation::new(range, self.domain, self.to_rule());
        r.allow_empty = self.allow_empty;
        r.show_dropdown = self.show_dropdown;
        r.style = self.style;
        r.message = (!self.message.trim().is_empty()).then(|| self.message.trim().to_string());
        r.title = (!self.title.trim().is_empty()).then(|| self.title.trim().to_string());
        r
    }

    /// Why OK is disabled, in the user's terms.
    ///
    /// A greyed button with no explanation is indistinguishable from a bug, so
    /// every reason here is a sentence.
    pub fn problem(&self) -> Option<&'static str> {
        match self.domain {
            ValueDomain::List if self.list_values().is_empty() => {
                Some("Enter at least one allowed value, one per line or comma-separated.")
            }
            ValueDomain::Custom if self.formula.trim().is_empty() => {
                Some("Enter a formula. It must evaluate to TRUE for a cell to pass.")
            }
            ValueDomain::TextLength
            | ValueDomain::WholeNumber
            | ValueDomain::Decimal
            | ValueDomain::Date
                if matches!(self.compare, CompareKind::Between | CompareKind::NotBetween)
                    && self.min() > self.max() =>
            {
                Some("The minimum is greater than the maximum, so nothing can pass.")
            }
            _ => None,
        }
    }
}

// =================================================================== state ==

/// Which dialog is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValidationMode {
    New,
    /// Editing the rule at this index; committing replaces it in place.
    Edit(usize),
    Manage,
}

/// The editor's whole state. `None` inside the app when nothing is open.
#[derive(Clone, PartialEq, Debug)]
pub struct ValidationState {
    /// The rectangle a new rule would cover.
    pub range: TableRange,
    pub mode: ValidationMode,
    pub form: ValidationForm,
    /// Where OK and Cancel were actually painted last frame, so a test can
    /// click the REAL buttons rather than the handler behind them. Same
    /// discipline as `CondFormatState`.
    pub ok_rect: Option<egui::Rect>,
    pub cancel_rect: Option<egui::Rect>,
}

impl ValidationState {
    pub fn new_rule(range: TableRange) -> Self {
        Self {
            range,
            mode: ValidationMode::New,
            form: ValidationForm::default(),
            ok_rect: None,
            cancel_rect: None,
        }
    }

    pub fn manage(range: TableRange) -> Self {
        Self {
            range,
            mode: ValidationMode::Manage,
            form: ValidationForm::default(),
            ok_rect: None,
            cancel_rect: None,
        }
    }
}

/// What the dialog asked the app to do.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ValidationOutcome {
    pub commit: bool,
    pub cancel: bool,
    pub delete: Option<usize>,
    pub edit: Option<usize>,
    pub new_rule: bool,
    pub back: bool,
}

impl ValidationOutcome {
    pub fn is_empty(&self) -> bool {
        *self == ValidationOutcome::default()
    }
}

// ====================================================================== ui ==

/// Draw the editor. `store` is READ-ONLY; every mutation is reported back.
pub fn show(
    ctx: &egui::Context,
    st: &mut ValidationState,
    store: &SheetValidation,
    th: Theme,
) -> ValidationOutcome {
    let mut out = ValidationOutcome::default();
    let title = match st.mode {
        ValidationMode::Manage => "Data Validation — Manage Rules",
        ValidationMode::Edit(_) => "Data Validation — Edit Rule",
        ValidationMode::New => "Data Validation — New Rule",
    };
    let mut open = true;
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(470.0)
        .show(ctx, |ui| match st.mode {
            ValidationMode::Manage => show_manage(ui, st, store, th, &mut out),
            _ => show_form(ui, st, th, &mut out),
        });
    if !open {
        out.cancel = true;
    }
    out
}

fn show_manage(
    ui: &mut egui::Ui,
    st: &mut ValidationState,
    store: &SheetValidation,
    th: Theme,
    out: &mut ValidationOutcome,
) {
    ui.label(
        RichText::new(format!(
            "Rules on this sheet · selection {}",
            st.range.to_a1()
        ))
        .color(th.text_dim),
    );
    ui.separator();
    if store.is_empty() {
        ui.label(
            RichText::new("No validation rules on this sheet yet.")
                .color(th.text_dim)
                .italics(),
        );
    } else {
        ui.label(
            RichText::new(
                "Listed in precedence order. A LATER rule wins on any cell both rules cover.",
            )
            .color(th.text_dim)
            .small(),
        );
        ui.add_space(4.0);
        egui::Grid::new("dv_rules")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                for (i, rule) in store.rules().iter().enumerate() {
                    ui.label(RichText::new(rule.range.to_a1()).strong());
                    ui.label(rule.domain.label());
                    // The lossy-export warning lives beside the rule, so a
                    // rule made before this build still announces itself.
                    let loss = ferrix_io::sheet_validation_xlsx_loss(rule);
                    match loss.first() {
                        Some(w) => {
                            ui.label(RichText::new("⚠ xlsx").color(th.error))
                                .on_hover_text(w.clone());
                        }
                        None => {
                            ui.label("");
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Edit").clicked() {
                            out.edit = Some(i);
                        }
                        if ui.button("Delete").clicked() {
                            out.delete = Some(i);
                        }
                    });
                    ui.end_row();
                }
            });
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("New rule…").clicked() {
            out.new_rule = true;
        }
        let close = ui.button("Close");
        st.cancel_rect = Some(close.rect);
        if close.clicked() {
            out.cancel = true;
        }
    });
}

fn show_form(ui: &mut egui::Ui, st: &mut ValidationState, th: Theme, out: &mut ValidationOutcome) {
    ui.label(RichText::new(format!("Applies to {}", st.range.to_a1())).color(th.text_dim));
    ui.label(
        RichText::new(
            "Stored once for the whole range — a rule over a million rows costs \
             the same as one over ten.",
        )
        .color(th.text_dim)
        .small(),
    );
    ui.separator();

    let f = &mut st.form;
    ui.horizontal(|ui| {
        ui.label("Allow");
        egui::ComboBox::from_id_salt("dv_domain")
            .selected_text(f.domain.label())
            .show_ui(ui, |ui| {
                for d in ValueDomain::ALL {
                    ui.selectable_value(&mut f.domain, d, d.label());
                }
            });
    });

    match f.domain {
        ValueDomain::List => {
            ui.label("Allowed values (one per line or comma-separated):");
            ui.add(
                egui::TextEdit::multiline(&mut f.list_text)
                    .desired_rows(5)
                    .desired_width(f32::INFINITY),
            );
            ui.checkbox(&mut f.show_dropdown, "Show an in-cell dropdown");
        }
        ValueDomain::Custom => {
            ui.label("Formula — the cell passes when this is TRUE:");
            ui.add(
                egui::TextEdit::singleline(&mut f.formula)
                    .hint_text("=MOD(A1,2)=0")
                    .desired_width(f32::INFINITY),
            );
        }
        ValueDomain::Any => {
            ui.label(
                RichText::new("Any value passes. Use this to attach only a message.")
                    .color(th.text_dim),
            );
        }
        _ => {
            ui.horizontal(|ui| {
                ui.label("Data");
                egui::ComboBox::from_id_salt("dv_compare")
                    .selected_text(f.compare.label())
                    .show_ui(ui, |ui| {
                        for k in CompareKind::ALL {
                            ui.selectable_value(&mut f.compare, k, k.label());
                        }
                    });
                if f.compare == CompareKind::Compare {
                    egui::ComboBox::from_id_salt("dv_op")
                        .selected_text(f.op.as_xlsx())
                        .show_ui(ui, |ui| {
                            for op in [
                                CmpOp::Eq,
                                CmpOp::Ne,
                                CmpOp::Lt,
                                CmpOp::Le,
                                CmpOp::Gt,
                                CmpOp::Ge,
                            ] {
                                ui.selectable_value(&mut f.op, op, op.as_xlsx());
                            }
                        });
                }
            });
            if f.compare != CompareKind::Any {
                ui.horizontal(|ui| {
                    ui.label(if f.compare == CompareKind::Compare {
                        "Value"
                    } else {
                        "Minimum"
                    });
                    ui.add(egui::TextEdit::singleline(&mut f.min_text).desired_width(90.0));
                    if f.compare != CompareKind::Compare {
                        ui.label("Maximum");
                        ui.add(egui::TextEdit::singleline(&mut f.max_text).desired_width(90.0));
                    }
                });
            }
        }
    }

    ui.add_space(6.0);
    ui.checkbox(&mut f.allow_empty, "Ignore blank cells");
    ui.separator();

    ui.horizontal(|ui| {
        ui.label("On invalid entry");
        egui::ComboBox::from_id_salt("dv_style")
            .selected_text(f.style.label())
            .show_ui(ui, |ui| {
                for s in ErrorStyle::ALL {
                    ui.selectable_value(&mut f.style, s, s.label());
                }
            });
    });
    ui.horizontal(|ui| {
        ui.label("Title");
        ui.add(egui::TextEdit::singleline(&mut f.title).desired_width(180.0));
    });
    ui.label("Message shown when an entry fails:");
    ui.add(
        egui::TextEdit::multiline(&mut f.message)
            .desired_rows(2)
            .desired_width(f32::INFINITY),
    );

    let problem = f.problem();
    if let Some(p) = problem {
        ui.add_space(4.0);
        ui.label(RichText::new(p).color(th.error));
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let ok = ui.add_enabled(problem.is_none(), egui::Button::new("OK"));
        st.ok_rect = Some(ok.rect);
        if ok.clicked() {
            out.commit = true;
        }
        let cancel = ui.button("Cancel");
        st.cancel_rect = Some(cancel.rect);
        if cancel.clicked() {
            out.cancel = true;
        }
        if ui.button("Back to list").clicked() {
            out.back = true;
        }
    });
}
