//! The conditional-formatting editor (roadmap #11).
//!
//! Everything below the UI already existed: [`ConditionalRule`] evaluates,
//! [`SheetFormat`] stores rules per column or per range, the grid paints what
//! `SheetFormat::resolve` returns, and `table_xlsx` round-trips most of it.
//! What did not exist was any way for a user to *make* a rule. This module is
//! that way, and nothing else — it owns no evaluation and no storage.
//!
//! ## Scope, and why a rule is never per cell
//!
//! A rule is attached to a **column** or to a **range**, never to a cell. That
//! is not a UI convenience, it is the invariant the whole formatting layer is
//! built on: at 200M rows a per-cell store for "colour this column" is ~9.6 GB
//! to express six bytes of fact. So [`CondTarget`] has exactly two shapes, and
//! `apply` writes exactly one entry however many cells the user had selected.
//! `rule_applied_to_a_100k_row_column_stores_one_entry` asserts it.
//!
//! ## Live preview, and why it is a *pending rule* rather than a written one
//!
//! The dialog shows its effect on the real grid before the user commits. The
//! obvious implementation — write the rule, remove it on Cancel — is wrong: a
//! removal has to reconstruct the exact prior state, and any bug in that
//! reconstruction silently corrupts the user's sheet on a *cancel*, which is
//! the one action that must be incapable of changing anything.
//!
//! So the preview is a separate, additive thing. [`CondFormatState::preview`]
//! holds the rule being edited; the app splices it into a CLONE of the sheet's
//! `SheetFormat` for painting only, and the real store is not touched until OK
//! is pressed. Cancel therefore does not "undo" anything — there was never
//! anything to undo, which is why `cancel_leaves_the_sheet_untouched` can
//! compare the whole `SheetFormat` for equality and expect it to hold.
//!
//! ## Precedence
//!
//! Later rules win, matching `apply_cell` and Excel. The Manage list is drawn
//! in storage order with the winner marked, and ▲/▼ move an entry through
//! `SheetFormat::move_column_rule` / `move_range_rule` — the same order the
//! resolver walks, not a second opinion about it.

use egui::{Color32, RichText};
use ferrix_core::{
    format::{ManualStyle, Typography},
    CmpOp, ConditionalRule, Rgb, SheetFormat, TableRange,
};

use crate::theme::Theme;

// ==================================================================== target ==

/// What a rule is being attached to.
///
/// Two shapes, deliberately: these are the two scopes `SheetFormat` stores.
/// There is no cell variant because there is no cell scope for rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CondTarget {
    /// Every row of a sheet column, including rows that do not exist yet.
    Column(u32),
    /// An explicit rectangle.
    Range(TableRange),
}

impl CondTarget {
    /// The target a selection implies.
    ///
    /// A selection spanning one column and more than one row is offered as a
    /// RANGE, not a column: the user drew a rectangle and expects the rule to
    /// stop where they stopped. Whole-column intent is expressed by choosing
    /// "Entire column" in the dialog, which is what [`CondTarget::widen`] does.
    pub fn from_selection(a: ferrix_core::CellRef, b: ferrix_core::CellRef) -> Self {
        CondTarget::Range(TableRange::new(a.row, a.col, b.row, b.col))
    }

    /// The whole-column form of this target, using its first column.
    pub fn widen(self) -> Self {
        match self {
            CondTarget::Column(c) => CondTarget::Column(c),
            CondTarget::Range(r) => CondTarget::Column(r.first_col),
        }
    }

    pub fn label(&self) -> String {
        match self {
            CondTarget::Column(c) => {
                format!("column {}", ferrix_core::column_name(*c))
            }
            CondTarget::Range(r) => r.to_a1(),
        }
    }

    /// The rules currently on this target, in precedence order.
    pub fn rules<'a>(&self, fmt: &'a SheetFormat) -> &'a [ConditionalRule] {
        match self {
            CondTarget::Column(c) => fmt.column_rules(*c),
            CondTarget::Range(r) => fmt.rules_for_range(*r),
        }
    }

    /// Append `rule`, creating the storage entry if this is the first one.
    ///
    /// ONE entry, whatever the target's row count.
    pub fn push(&self, fmt: &mut SheetFormat, rule: ConditionalRule) {
        match self {
            CondTarget::Column(c) => {
                fmt.push_column_rule(*c, rule);
            }
            CondTarget::Range(r) => {
                fmt.push_rule_for_range(*r, rule);
            }
        }
    }

    /// Replace rule `i` in place, keeping its precedence position.
    pub fn replace(&self, fmt: &mut SheetFormat, i: usize, rule: ConditionalRule) -> bool {
        match self {
            CondTarget::Column(c) => fmt.set_column_rule(*c, i, rule),
            CondTarget::Range(r) => match fmt.range_index_of(*r) {
                Some(ri) => fmt.set_range_rule(ri, i, rule),
                None => false,
            },
        }
    }

    pub fn remove(&self, fmt: &mut SheetFormat, i: usize) -> bool {
        match self {
            CondTarget::Column(c) => fmt.remove_column_rule(*c, i).is_some(),
            CondTarget::Range(r) => match fmt.range_index_of(*r) {
                Some(ri) => fmt.remove_range_rule(ri, i).is_some(),
                None => false,
            },
        }
    }

    /// Move rule `i` by `delta` places in the precedence order.
    pub fn move_rule(&self, fmt: &mut SheetFormat, i: usize, delta: isize) -> bool {
        match self {
            CondTarget::Column(c) => fmt.move_column_rule(*c, i, delta),
            CondTarget::Range(r) => match fmt.range_index_of(*r) {
                Some(ri) => fmt.move_range_rule(ri, i, delta),
                None => false,
            },
        }
    }
}

// ====================================================================== kinds ==

/// The rule variants the editor offers, one per [`ConditionalRule`] case.
///
/// A flat enum rather than reading the discriminant off a `ConditionalRule`,
/// because the dialog has to remember which kind the user picked *while the
/// fields for it are still half-filled* — and a `ConditionalRule` with a
/// half-filled threshold is not a thing that should exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuleKind {
    Threshold,
    ColorScale2,
    ColorScale3,
    DataBar,
    Sign,
    TopBottom,
    TextContains,
    Manual,
}

impl RuleKind {
    pub const ALL: [RuleKind; 8] = [
        RuleKind::Threshold,
        RuleKind::ColorScale2,
        RuleKind::ColorScale3,
        RuleKind::DataBar,
        RuleKind::Sign,
        RuleKind::TopBottom,
        RuleKind::TextContains,
        RuleKind::Manual,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RuleKind::Threshold => "Cell value",
            RuleKind::ColorScale2 => "2-colour scale",
            RuleKind::ColorScale3 => "3-colour scale",
            RuleKind::DataBar => "Data bar",
            RuleKind::Sign => "Colour by sign",
            RuleKind::TopBottom => "Top / bottom N",
            RuleKind::TextContains => "Text contains",
            RuleKind::Manual => "Fill / text colour",
        }
    }

    pub fn of(rule: &ConditionalRule) -> RuleKind {
        match rule {
            ConditionalRule::Threshold { .. } => RuleKind::Threshold,
            ConditionalRule::ColorScale2 { .. } => RuleKind::ColorScale2,
            ConditionalRule::ColorScale3 { .. } => RuleKind::ColorScale3,
            ConditionalRule::DataBar { .. } => RuleKind::DataBar,
            ConditionalRule::Sign { .. } => RuleKind::Sign,
            ConditionalRule::TopBottom { .. } => RuleKind::TopBottom,
            ConditionalRule::TextContains { .. } => RuleKind::TextContains,
            ConditionalRule::Manual { .. } => RuleKind::Manual,
        }
    }
}

// ======================================================================= form ==

/// The dialog's live field values.
///
/// Every kind's fields are held at once rather than in a per-kind enum, so
/// flipping the kind selector back and forth does not discard what the user
/// typed. That is the difference between a dialog that feels like a dialog and
/// one that punishes exploring.
#[derive(Clone, PartialEq, Debug)]
pub struct RuleForm {
    pub kind: RuleKind,
    // Threshold
    pub op: CmpOp,
    pub value: f64,
    /// The threshold value as typed. Parsed on commit so a half-typed "-" or
    /// "1e" does not snap the field back to 0 mid-keystroke.
    pub value_text: String,
    // shared highlight colours (Threshold / TopBottom / TextContains)
    pub fill: Rgb,
    pub text: Rgb,
    // scales & bars
    pub scale_min: Rgb,
    pub scale_mid: Rgb,
    pub scale_max: Rgb,
    pub bar: Rgb,
    // sign
    pub negative: Option<Rgb>,
    pub positive: Option<Rgb>,
    pub zero: Option<Rgb>,
    // top/bottom
    pub top: bool,
    pub n: u32,
    // text contains
    pub needle: String,
    // manual
    pub manual_fill: Option<Rgb>,
    pub manual_text: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
}

impl Default for RuleForm {
    fn default() -> Self {
        Self {
            kind: RuleKind::Threshold,
            op: CmpOp::Gt,
            value: 0.0,
            value_text: "0".into(),
            fill: Rgb(0xC6, 0xEF, 0xCE),
            text: Rgb(0x00, 0x61, 0x00),
            scale_min: Rgb(0xFF, 0xFF, 0xFF),
            scale_mid: Rgb(0xFF, 0xEB, 0x84),
            scale_max: Rgb(0x53, 0x8D, 0xD5),
            bar: Rgb(0x63, 0x8E, 0xC6),
            negative: Some(Rgb(0xC0, 0x28, 0x28)),
            positive: Some(Rgb(0x1E, 0x88, 0x3C)),
            zero: None,
            top: true,
            n: 10,
            needle: String::new(),
            manual_fill: Some(Rgb(0xFF, 0xEB, 0x9C)),
            manual_text: None,
            bold: false,
            italic: false,
        }
    }
}

impl RuleForm {
    /// Seed the form from an existing rule, so Edit opens on what is there
    /// rather than on the defaults.
    pub fn from_rule(rule: &ConditionalRule) -> Self {
        let mut f = RuleForm {
            kind: RuleKind::of(rule),
            ..Default::default()
        };
        match rule {
            ConditionalRule::Threshold {
                op,
                value,
                fill,
                text,
            } => {
                f.op = *op;
                f.value = *value;
                f.value_text = ferrix_core::format_number(*value);
                f.fill = *fill;
                f.text = *text;
            }
            ConditionalRule::ColorScale2 { min, max } => {
                f.scale_min = *min;
                f.scale_max = *max;
            }
            ConditionalRule::ColorScale3 { min, mid, max } => {
                f.scale_min = *min;
                f.scale_mid = *mid;
                f.scale_max = *max;
            }
            ConditionalRule::DataBar { color } => f.bar = *color,
            ConditionalRule::Sign {
                negative,
                positive,
                zero,
            } => {
                f.negative = *negative;
                f.positive = *positive;
                f.zero = *zero;
            }
            ConditionalRule::TopBottom { top, n, fill, text } => {
                f.top = *top;
                f.n = *n;
                f.fill = *fill;
                f.text = *text;
            }
            ConditionalRule::TextContains { needle, fill, text } => {
                f.needle.clone_from(needle);
                f.fill = *fill;
                f.text = *text;
            }
            ConditionalRule::Manual {
                fill,
                text,
                typography,
            } => {
                f.manual_fill = *fill;
                f.manual_text = *text;
                f.bold = typography.bold.unwrap_or(false);
                f.italic = typography.italic.unwrap_or(false);
            }
        }
        f
    }

    /// The rule this form currently describes.
    ///
    /// Total: every field combination produces a rule. A vacuous one (an empty
    /// text needle, a Sign with no colours) is *allowed* to be built here and
    /// refused by [`RuleForm::problem`] instead, so the preview can keep
    /// updating while the user is still typing the needle.
    pub fn to_rule(&self) -> ConditionalRule {
        match self.kind {
            RuleKind::Threshold => ConditionalRule::Threshold {
                op: self.op,
                value: self.value,
                fill: self.fill,
                text: self.text,
            },
            RuleKind::ColorScale2 => ConditionalRule::ColorScale2 {
                min: self.scale_min,
                max: self.scale_max,
            },
            RuleKind::ColorScale3 => ConditionalRule::ColorScale3 {
                min: self.scale_min,
                mid: self.scale_mid,
                max: self.scale_max,
            },
            RuleKind::DataBar => ConditionalRule::DataBar { color: self.bar },
            RuleKind::Sign => ConditionalRule::Sign {
                negative: self.negative,
                positive: self.positive,
                zero: self.zero,
            },
            RuleKind::TopBottom => ConditionalRule::TopBottom {
                top: self.top,
                // Zero would mean "highlight nothing", which no user ever
                // means; it is clamped rather than refused so the spinner can
                // pass through 0 without the dialog erroring at them.
                n: self.n.max(1),
                fill: self.fill,
                text: self.text,
            },
            RuleKind::TextContains => ConditionalRule::TextContains {
                needle: self.needle.clone(),
                fill: self.fill,
                text: self.text,
            },
            RuleKind::Manual => ConditionalRule::Manual {
                fill: self.manual_fill,
                text: self.manual_text,
                typography: Typography {
                    bold: self.bold.then_some(true),
                    italic: self.italic.then_some(true),
                    ..Default::default()
                },
            },
        }
    }

    /// Why this rule cannot be saved yet, if it cannot.
    ///
    /// Only genuinely inert rules are refused. A rule that *can* match nothing
    /// today (a threshold no row meets) is perfectly legal — the data may
    /// change — but one that can never match anything is a rule the user will
    /// believe is working, and that is worth blocking.
    pub fn problem(&self) -> Option<&'static str> {
        match self.kind {
            RuleKind::TextContains if self.needle.trim().is_empty() => {
                Some("Enter the text to look for — an empty needle matches nothing.")
            }
            RuleKind::Sign
                if self.negative.is_none() && self.positive.is_none() && self.zero.is_none() =>
            {
                Some("Pick at least one colour, or this rule does nothing.")
            }
            RuleKind::Manual if self.manual_fill.is_none() && self.manual_text.is_none() => {
                Some("Pick a fill or a text colour.")
            }
            _ => None,
        }
    }
}

// ====================================================================== state ==

/// Which dialog is open, if any.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CondMode {
    /// Creating a rule. Committing appends it.
    New,
    /// Editing the rule at this index. Committing replaces it in place, so its
    /// precedence position is preserved.
    Edit(usize),
    /// The rules list for the target.
    Manage,
}

/// The editor's whole state. `None` inside the app when nothing is open.
#[derive(Clone, PartialEq, Debug)]
pub struct CondFormatState {
    pub target: CondTarget,
    pub mode: CondMode,
    pub form: RuleForm,
    /// True while the dialog wants its rule painted but not stored. Cleared on
    /// both OK and Cancel; the app reads it to decide whether to build the
    /// preview overlay at all.
    pub preview: bool,
}

impl CondFormatState {
    pub fn new_rule(target: CondTarget) -> Self {
        Self {
            target,
            mode: CondMode::New,
            form: RuleForm::default(),
            preview: true,
        }
    }

    pub fn manage(target: CondTarget) -> Self {
        Self {
            target,
            mode: CondMode::Manage,
            form: RuleForm::default(),
            preview: false,
        }
    }

    /// The rule the grid should paint speculatively, if any.
    ///
    /// `Manage` returns `None`: the rules it lists are already stored and
    /// already painted, so previewing them again would double-apply them.
    pub fn preview_rule(&self) -> Option<ConditionalRule> {
        (self.preview && self.mode != CondMode::Manage).then(|| self.form.to_rule())
    }

    /// The `SheetFormat` the grid should paint from this frame.
    ///
    /// Returns `None` when there is nothing to preview, and the caller then
    /// paints the real store with no clone at all — the overwhelmingly common
    /// case, including every frame in which no dialog is open.
    ///
    /// When there IS something to preview it clones the format and splices the
    /// pending rule in at the position it would occupy once committed: appended
    /// for `New`, in place for `Edit`. Cloning a `SheetFormat` is cloning a
    /// handful of rules — it is a function of how many rules exist, never of
    /// how many rows they cover — and it happens only while a modal is open.
    pub fn preview_format(&self, base: &SheetFormat) -> Option<SheetFormat> {
        let rule = self.preview_rule()?;
        let mut f = base.clone();
        match self.mode {
            CondMode::Edit(i) if self.target.replace(&mut f, i, rule.clone()) => {}
            // An Edit whose index no longer exists (the rule was deleted from
            // under the dialog) degrades to showing it appended rather than
            // showing nothing, which is the honest preview of what OK would do.
            _ => self.target.push(&mut f, rule),
        }
        Some(f)
    }
}

// ======================================================================= xlsx ==

/// The warning to show for a rule that will not survive an xlsx export.
///
/// Sourced from `ferrix_io::table_xlsx::rule_survives_xlsx`, which is the same
/// predicate the exporter itself uses — so this cannot drift from what export
/// actually does, which is the entire point of asking it rather than
/// re-listing the lossy variants here.
pub fn xlsx_warning(rule: &ConditionalRule) -> Option<String> {
    if ferrix_io::table_xlsx::rule_survives_xlsx(rule) {
        return None;
    }
    Some(format!(
        "\"{}\" has no Excel equivalent and will be DROPPED when you export to \
         .xlsx. It keeps working in Ferrix.",
        rule.label()
    ))
}

// ========================================================================= ui ==

fn to_c32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.0, c.1, c.2)
}

fn from_c32(c: Color32) -> Rgb {
    Rgb(c.r(), c.g(), c.b())
}

/// A labelled colour swatch bound to an `Rgb`.
fn color_row(ui: &mut egui::Ui, label: &str, rgb: &mut Rgb) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut c = to_c32(*rgb);
        if ui.color_edit_button_srgba(&mut c).changed() {
            *rgb = from_c32(c);
        }
    });
}

/// A colour that can be switched off entirely — the Sign and Manual cases,
/// where "no colour" is a meaningful choice and not the same as white.
fn opt_color_row(ui: &mut egui::Ui, label: &str, slot: &mut Option<Rgb>) {
    ui.horizontal(|ui| {
        let mut on = slot.is_some();
        if ui.checkbox(&mut on, label).changed() {
            *slot = on.then(|| slot.unwrap_or(Rgb(0x80, 0x80, 0x80)));
        }
        if let Some(rgb) = slot.as_mut() {
            let mut c = to_c32(*rgb);
            if ui.color_edit_button_srgba(&mut c).changed() {
                *rgb = from_c32(c);
            }
        }
    });
}

/// What the dialog asked the app to do, once the window has closed.
///
/// Returned rather than applied in place because the dialog borrows the app's
/// `SheetFormat` to list rules, and committing mutates it.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct CondOutcome {
    /// Commit the form: append (New) or replace at index (Edit).
    pub commit: bool,
    /// Close without changing anything.
    pub cancel: bool,
    /// Delete the rule at this index.
    pub delete: Option<usize>,
    /// Move the rule at `.0` by `.1` places.
    pub move_rule: Option<(usize, isize)>,
    /// Open the editor on the rule at this index.
    pub edit: Option<usize>,
    /// Open the New Rule form.
    pub new_rule: bool,
    /// Switch the target between column and range scope.
    pub retarget: Option<CondTarget>,
    /// Go back to the Manage list.
    pub back: bool,
}

impl CondOutcome {
    pub fn is_empty(&self) -> bool {
        *self == CondOutcome::default()
    }
}

/// Draw the editor. `fmt` is READ-ONLY here; every mutation is reported back
/// through [`CondOutcome`] and applied by the caller.
pub fn show(
    ctx: &egui::Context,
    st: &mut CondFormatState,
    fmt: &SheetFormat,
    th: Theme,
) -> CondOutcome {
    let mut out = CondOutcome::default();
    let title = match st.mode {
        CondMode::Manage => "Conditional Formatting — Manage Rules",
        CondMode::Edit(_) => "Conditional Formatting — Edit Rule",
        CondMode::New => "Conditional Formatting — New Rule",
    };
    // `open` is the window's own ✖. Treated exactly as Cancel: closing a
    // preview dialog by any route must leave the sheet alone.
    let mut open = true;
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(460.0)
        .show(ctx, |ui| match st.mode {
            CondMode::Manage => show_manage(ui, st, fmt, th, &mut out),
            _ => show_form(ui, st, th, &mut out),
        });
    if !open {
        out.cancel = true;
    }
    out
}

fn show_manage(
    ui: &mut egui::Ui,
    st: &mut CondFormatState,
    fmt: &SheetFormat,
    th: Theme,
    out: &mut CondOutcome,
) {
    target_row(ui, st, th, out);
    ui.separator();

    let rules = st.target.rules(fmt);
    if rules.is_empty() {
        ui.label(
            RichText::new("No rules on this selection yet.")
                .color(th.text_dim)
                .italics(),
        );
    } else {
        ui.label(
            RichText::new(
                "Listed in precedence order. A LATER rule wins on any cell both \
                 rules match — use ▲ / ▼ to change which.",
            )
            .color(th.text_dim)
            .small(),
        );
        ui.add_space(4.0);
        egui::Grid::new("cf_rules")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                let last = rules.len() - 1;
                for (i, rule) in rules.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}.", i + 1)).color(th.text_dim));
                        ui.label(RichText::new(rule.label()).strong());
                        // The winner is marked rather than merely implied by
                        // position: "last wins" is exactly the fact users get
                        // wrong about every spreadsheet's rule list.
                        if i == last && rules.len() > 1 {
                            ui.label(RichText::new("wins").color(th.accent).small());
                        }
                    });

                    // The lossy-export warning lives beside the rule in the
                    // list, not only in the form — a rule created before this
                    // build, or edited by someone else, must still announce it.
                    match xlsx_warning(rule) {
                        Some(w) => {
                            ui.label(RichText::new("⚠ xlsx").color(th.error))
                                .on_hover_text(w);
                        }
                        None => {
                            ui.label("");
                        }
                    }

                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(i > 0, egui::Button::new("▲"))
                            .on_hover_text("Earlier — loses to the rules below it")
                            .clicked()
                        {
                            out.move_rule = Some((i, -1));
                        }
                        if ui
                            .add_enabled(i < last, egui::Button::new("▼"))
                            .on_hover_text("Later — wins over the rules above it")
                            .clicked()
                        {
                            out.move_rule = Some((i, 1));
                        }
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

    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("✚ New rule…").clicked() {
            out.new_rule = true;
        }
        if ui.button("Close").clicked() {
            out.cancel = true;
        }
    });
}

fn target_row(ui: &mut egui::Ui, st: &CondFormatState, th: Theme, out: &mut CondOutcome) {
    ui.horizontal(|ui| {
        ui.label("Applies to:");
        ui.label(
            RichText::new(st.target.label())
                .monospace()
                .color(th.accent),
        );
        let is_col = matches!(st.target, CondTarget::Column(_));
        if !is_col
            && ui
                .button("Entire column")
                .on_hover_text(
                    "Apply to every row of this column, including rows that do not \
                     exist yet. Still ONE stored entry.",
                )
                .clicked()
        {
            out.retarget = Some(st.target.widen());
        }
    });
}

fn show_form(ui: &mut egui::Ui, st: &mut CondFormatState, th: Theme, out: &mut CondOutcome) {
    target_row(ui, st, th, out);
    ui.separator();

    ui.horizontal(|ui| {
        ui.label("Rule type:");
        egui::ComboBox::from_id_salt("cf_kind")
            .selected_text(st.form.kind.label())
            .show_ui(ui, |ui| {
                for k in RuleKind::ALL {
                    ui.selectable_value(&mut st.form.kind, k, k.label());
                }
            });
    });
    ui.separator();

    let f = &mut st.form;
    match f.kind {
        RuleKind::Threshold => {
            ui.horizontal(|ui| {
                ui.label("Cell value");
                egui::ComboBox::from_id_salt("cf_op")
                    .selected_text(f.op.symbol())
                    .width(56.0)
                    .show_ui(ui, |ui| {
                        for op in [
                            CmpOp::Gt,
                            CmpOp::Ge,
                            CmpOp::Lt,
                            CmpOp::Le,
                            CmpOp::Eq,
                            CmpOp::Ne,
                        ] {
                            ui.selectable_value(&mut f.op, op, op.symbol());
                        }
                    });
                if ui
                    .add(egui::TextEdit::singleline(&mut f.value_text).desired_width(90.0))
                    .changed()
                {
                    // An unparseable in-progress entry leaves the last good
                    // value in place, so the live preview stays stable while
                    // the user types "-" or "1e" rather than flickering to 0.
                    if let Ok(v) = f.value_text.trim().parse::<f64>() {
                        f.value = v;
                    }
                }
            });
            color_row(ui, "Fill", &mut f.fill);
            color_row(ui, "Text", &mut f.text);
        }
        RuleKind::ColorScale2 => {
            scale_note(ui, th);
            color_row(ui, "Lowest", &mut f.scale_min);
            color_row(ui, "Highest", &mut f.scale_max);
        }
        RuleKind::ColorScale3 => {
            scale_note(ui, th);
            color_row(ui, "Lowest", &mut f.scale_min);
            color_row(ui, "Midpoint", &mut f.scale_mid);
            color_row(ui, "Highest", &mut f.scale_max);
        }
        RuleKind::DataBar => {
            scale_note(ui, th);
            color_row(ui, "Bar", &mut f.bar);
        }
        RuleKind::Sign => {
            ui.label(
                RichText::new(
                    "Sets the TEXT colour only — filling every negative cell drowns the sheet.",
                )
                .color(th.text_dim)
                .small(),
            );
            opt_color_row(ui, "Negative", &mut f.negative);
            opt_color_row(ui, "Positive", &mut f.positive);
            opt_color_row(ui, "Zero", &mut f.zero);
        }
        RuleKind::TopBottom => {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("cf_tb")
                    .selected_text(if f.top { "Top" } else { "Bottom" })
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut f.top, true, "Top");
                        ui.selectable_value(&mut f.top, false, "Bottom");
                    });
                ui.add(egui::DragValue::new(&mut f.n).range(1..=1000));
                ui.label("values");
            });
            scale_note(ui, th);
            color_row(ui, "Fill", &mut f.fill);
            color_row(ui, "Text", &mut f.text);
        }
        RuleKind::TextContains => {
            ui.horizontal(|ui| {
                ui.label("Text contains");
                ui.add(egui::TextEdit::singleline(&mut f.needle).desired_width(180.0));
            });
            ui.label(
                RichText::new("Compared case-insensitively against the cell's displayed text.")
                    .color(th.text_dim)
                    .small(),
            );
            color_row(ui, "Fill", &mut f.fill);
            color_row(ui, "Text", &mut f.text);
        }
        RuleKind::Manual => {
            ui.label(
                RichText::new(
                    "An unconditional colour — a rule whose condition is always true, \
                     so it takes part in the same precedence order as the rest.",
                )
                .color(th.text_dim)
                .small(),
            );
            opt_color_row(ui, "Fill", &mut f.manual_fill);
            opt_color_row(ui, "Text", &mut f.manual_text);
            ui.horizontal(|ui| {
                ui.checkbox(&mut f.bold, "Bold");
                ui.checkbox(&mut f.italic, "Italic");
            });
        }
    }

    ui.separator();

    // --- the xlsx warning ---
    //
    // Shown in the FORM, while the rule is still being made, because the whole
    // reason `rule_survives_xlsx` exists is so this is not discovered after
    // opening the file in Excel.
    let rule = st.form.to_rule();
    if let Some(w) = xlsx_warning(&rule) {
        ui.label(RichText::new(format!("⚠ {w}")).color(th.error));
        ui.add_space(2.0);
    }

    let problem = st.form.problem();
    if let Some(p) = problem {
        ui.label(RichText::new(p).color(th.text_dim));
    }

    ui.horizontal(|ui| {
        ui.checkbox(&mut st.preview, "Live preview")
            .on_hover_text("Show this rule on the grid before saving it. Cancel discards it.");
        ui.add_space(12.0);
        if ui
            .add_enabled(problem.is_none(), egui::Button::new("OK"))
            .clicked()
        {
            out.commit = true;
        }
        if ui.button("Cancel").clicked() {
            out.cancel = true;
        }
        if matches!(st.mode, CondMode::Edit(_)) && ui.button("Back to rules").clicked() {
            out.back = true;
        }
    });
}

fn scale_note(ui: &mut egui::Ui, th: Theme) {
    ui.label(
        RichText::new(
            "Scaled over the rows currently ON SCREEN — an exact answer would mean \
             scanning every row on every repaint. Scrolling rescales it.",
        )
        .color(th.text_dim)
        .small(),
    );
}

/// A manual style as the toolbar would express it, for callers that want to
/// turn the Manual form into a `ManualStyle` rather than a rule.
pub fn manual_of(form: &RuleForm) -> ManualStyle {
    ManualStyle {
        fill: form.manual_fill,
        text: form.manual_text,
        typography: Typography {
            bold: form.bold.then_some(true),
            italic: form.italic.then_some(true),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests;
