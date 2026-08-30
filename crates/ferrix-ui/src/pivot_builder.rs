//! The pivot builder panel: assemble a [`PivotSpec`] by dragging source
//! columns into wells (issue #33 Part C).
//!
//! ## What lives here and what does not
//!
//! Modelled on [`crate::validation_panel`] and [`crate::cond_format`], for the
//! same reason those exist: the aggregation kernel (`ferrix_core::pivot`, Part
//! A) and the pivot SHEET (source ref + spec + refresh + persistence, Part B)
//! are complete on their own, and what was missing was any way for a user to
//! *build* a spec without the API. This module is that way.
//!
//! It owns NO aggregation, NO sheet creation, and NO storage. Every commit is
//! reported back through [`PivotBuilderOutcome`] and applied by `app.rs`, which
//! funnels through Part B's `add_sheet` + `set_pivot` + `refresh_pivot` — the
//! builder never reimplements them. Editing an existing pivot refreshes the
//! SAME sheet rather than making a new one (`editing` carries that sheet id).
//!
//! ## Value-free, like the kernel
//!
//! A well holds [`ferrix_core::ColIdx`]s and [`ferrix_core::PivotAgg`]s —
//! exactly the two slices `compute_pivot` takes — never a
//! `ferrix_core::Value`. The kernel is deliberately `Value`-free (see the
//! comment atop `pivot.rs`); coupling the builder to `Value` would undo that.
//!
//! ## The drop methods are the real entry point
//!
//! `drop_row`, `drop_value`, the reorder and remove methods mutate the wells.
//! The egui drag-and-drop handlers below call exactly these, and so do the
//! harness tests — one path, not two. This follows the harness's stated
//! discipline: a test about SPEC ASSEMBLY should drive the same methods the UI
//! drives, not synthesise pixel drags whose arithmetic is what breaks (the
//! four false bug reports this repo already has from synthetic egui input).

use eframe::egui::{self, RichText};
use ferrix_core::{ColIdx, PivotAgg};

use crate::theme::Theme;
use crate::workbook::PivotSpec;

/// The six aggregates the builder offers, in menu order, with their labels.
///
/// A local table rather than a `PivotAgg::ALL`: the kernel enum is deliberately
/// minimal and carries no display concern, and this is exactly the set the
/// acceptance criteria name (Sum/Count/Avg/Min/Max/StdDev). Keeping the label
/// beside the variant means the dropdown and the committed spec cannot disagree
/// about what "Avg" means.
pub const AGGS: [(PivotAgg, &str); 6] = [
    (PivotAgg::Sum, "Sum"),
    (PivotAgg::Count, "Count"),
    (PivotAgg::Avg, "Avg"),
    (PivotAgg::Min, "Min"),
    (PivotAgg::Max, "Max"),
    (PivotAgg::StdDev, "StdDev"),
];

/// The short label for an aggregate, for the value chip and the dropdown.
pub fn agg_label(agg: PivotAgg) -> &'static str {
    AGGS.iter()
        .find(|(a, _)| *a == agg)
        .map(|(_, l)| *l)
        .unwrap_or("Sum")
}

/// One value field: a source column and how to aggregate it.
///
/// A separate struct from the kernel's `(ColIdx, PivotAgg)` tuple only so the
/// UI has a place to hang per-chip state should it ever need one; `as_pair`
/// projects it back to exactly what [`PivotSpec::values`] and the kernel take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueField {
    pub col: ColIdx,
    pub agg: PivotAgg,
}

impl ValueField {
    pub fn as_pair(self) -> (ColIdx, PivotAgg) {
        (self.col, self.agg)
    }
}

/// What the panel reported this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PivotBuilderOutcome {
    /// Nothing to act on.
    #[default]
    None,
    /// The user pressed "Create pivot" / "Update pivot".
    Commit,
    /// The user closed the panel without committing.
    Cancel,
}

/// The builder's live state: the source, the wells, and (when editing) the
/// pivot sheet being reshaped.
///
/// Bounded by the source's COLUMN count, never its row count: `columns` holds
/// one entry per source column, and the wells hold at most that many. A pivot
/// over a 200M-row source builds the same-size state as one over ten rows —
/// the scale invariant carried into the builder.
#[derive(Clone, Debug)]
pub struct PivotBuilderState {
    /// The sheet the pivot reads.
    pub source: ferrix_core::SheetId,
    /// The pivot sheet being EDITED, or `None` when building a fresh one. This
    /// is what makes "edit the spec later refreshes the same sheet" hold: on
    /// commit, `app.rs` reuses this id instead of adding a sheet.
    pub editing: Option<ferrix_core::SheetId>,
    /// Every source column: its index and the header/letter to show on its
    /// chip. Populated once when the panel opens.
    pub columns: Vec<(ColIdx, String)>,
    /// Group-by columns, in order. Reorder is meaningful: it sets the nesting
    /// order of the pivot's row groups.
    pub rows: Vec<ColIdx>,
    /// Value fields, in order.
    pub values: Vec<ValueField>,
    /// The aggregate a freshly dropped value column gets. Defaults to Sum,
    /// which is what a spreadsheet user expects; changeable per chip after.
    pub default_agg: PivotAgg,
}

impl PivotBuilderState {
    /// Open the builder over `source`, listing `columns` as `(index, label)`.
    ///
    /// `editing` is `Some` when reshaping an existing pivot (so commit refreshes
    /// that sheet) and `None` for a fresh build. When editing, `rows`/`values`
    /// are seeded from the existing spec by the caller via [`Self::seed`].
    pub fn new(
        source: ferrix_core::SheetId,
        columns: Vec<(ColIdx, String)>,
        editing: Option<ferrix_core::SheetId>,
    ) -> Self {
        Self {
            source,
            editing,
            columns,
            rows: Vec::new(),
            values: Vec::new(),
            default_agg: PivotAgg::Sum,
        }
    }

    /// Fill the wells from an existing spec, for the edit path.
    pub fn seed(&mut self, spec: &PivotSpec) {
        self.rows = spec.group_by.clone();
        self.values = spec
            .values
            .iter()
            .map(|&(col, agg)| ValueField { col, agg })
            .collect();
    }

    /// The label to show for a source column, falling back to its letter.
    pub fn label_of(&self, col: ColIdx) -> String {
        self.columns
            .iter()
            .find(|(c, _)| *c == col)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| ferrix_core::column_name(col.0))
    }

    /// Add `col` to the Rows well, if it is not already there.
    ///
    /// A group-by column appearing twice would make a redundant nesting level,
    /// so a repeat drop is ignored rather than duplicated — the same column can
    /// still be a Value (grouping by it and counting it is meaningful).
    pub fn drop_row(&mut self, col: ColIdx) {
        if !self.rows.contains(&col) {
            self.rows.push(col);
        }
    }

    /// Add `col` to the Values well with `agg`.
    ///
    /// Duplicates ARE allowed here: Sum and Avg of the same column are two
    /// distinct, useful fields, so a value well is a list, not a set.
    pub fn drop_value(&mut self, col: ColIdx, agg: PivotAgg) {
        self.values.push(ValueField { col, agg });
    }

    /// Remove the Rows entry at `i`.
    pub fn remove_row(&mut self, i: usize) {
        if i < self.rows.len() {
            self.rows.remove(i);
        }
    }

    /// Remove the Values entry at `i`.
    pub fn remove_value(&mut self, i: usize) {
        if i < self.values.len() {
            self.values.remove(i);
        }
    }

    /// Move a Rows entry from `from` to `to`, clamping `to` into range. Used by
    /// reorder-within-a-well; a no-op when the indices coincide.
    pub fn reorder_row(&mut self, from: usize, to: usize) {
        reorder(&mut self.rows, from, to);
    }

    /// Move a Values entry from `from` to `to`.
    pub fn reorder_value(&mut self, from: usize, to: usize) {
        reorder(&mut self.values, from, to);
    }

    /// Change the aggregate on the value chip at `i`.
    pub fn set_value_agg(&mut self, i: usize, agg: PivotAgg) {
        if let Some(v) = self.values.get_mut(i) {
            v.agg = agg;
        }
    }

    /// Project the wells to the spec the kernel and Part B take.
    pub fn spec(&self) -> PivotSpec {
        PivotSpec {
            group_by: self.rows.clone(),
            values: self.values.iter().map(|v| v.as_pair()).collect(),
        }
    }

    /// A spec is committable once it has at least one Row or one Value — an
    /// empty spec would compute a single blank group, which is never what the
    /// user meant to create.
    pub fn is_committable(&self) -> bool {
        !self.rows.is_empty() || !self.values.is_empty()
    }
}

/// Move `v[from]` to index `to`, shifting the rest. Clamps `to`; no-op when the
/// vector is too short or the move is a self-move.
fn reorder<T>(v: &mut Vec<T>, from: usize, to: usize) {
    if from >= v.len() || from == to {
        return;
    }
    let item = v.remove(from);
    let to = to.min(v.len());
    v.insert(to, item);
}

// A drag payload is just the source column index. egui's dnd carries it typed;
// keeping it a bare `ColIdx` means the drop handler needs no downcasting.
type DragPayload = ColIdx;

/// Draw the pivot builder as a right-hand side panel and report what the user
/// did. `st` is mutated in place by drags, reorders and removals.
///
/// A first-class `SidePanel`, not a floating `Window`: the acceptance criteria
/// ask for a panel, and a panel docks beside the grid so the source range stays
/// visible while the user drags from it.
pub fn show(ctx: &egui::Context, st: &mut PivotBuilderState, th: Theme) -> PivotBuilderOutcome {
    let mut outcome = PivotBuilderOutcome::None;

    egui::SidePanel::right("ferrix_pivot_builder")
        .resizable(true)
        .default_width(280.0)
        .frame(egui::Frame::none().fill(th.panel).inner_margin(10.0))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if st.editing.is_some() {
                        "Edit pivot"
                    } else {
                        "Pivot builder"
                    })
                    .color(th.accent)
                    .strong()
                    .size(15.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("✕")
                        .on_hover_text("Close without saving")
                        .clicked()
                    {
                        outcome = PivotBuilderOutcome::Cancel;
                    }
                });
            });
            ui.label(
                RichText::new("Drag columns from the list into a well.")
                    .color(th.text_dim)
                    .small(),
            );
            ui.separator();

            // --- source columns (drag sources) ---
            ui.label(RichText::new("Columns").color(th.text).strong());
            ui.horizontal_wrapped(|ui| {
                for &(col, ref label) in &st.columns {
                    let id = egui::Id::new(("ferrix_pivot_src", col.0));
                    ui.dnd_drag_source(id, col, |ui| {
                        chip(ui, th, label, None);
                    });
                }
            });
            ui.separator();

            // --- Rows well ---
            ui.label(RichText::new("Rows").color(th.text).strong())
                .on_hover_text("Group the source by these columns, in order.");
            rows_well(ui, st, th);

            ui.add_space(6.0);

            // --- Columns well (stub: unsupported by the Part A kernel) ---
            ui.add_enabled_ui(false, |ui| {
                ui.label(RichText::new("Columns").color(th.text_dim).strong());
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 34.0),
                    egui::Sense::hover(),
                );
                ui.painter()
                    .rect_stroke(rect, 4.0, egui::Stroke::new(1.0_f32, th.grid_line));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "not yet supported",
                    egui::FontId::proportional(11.0),
                    th.text_dim,
                );
            })
            .response
            .on_hover_text(
                "A column axis needs kernel support that isn't in this build yet. \
                 Use Rows and Values for now.",
            );

            ui.add_space(6.0);

            // --- Values well ---
            ui.label(RichText::new("Values").color(th.text).strong())
                .on_hover_text("Aggregate these columns per group.");
            values_well(ui, st, th);

            ui.separator();
            let can = st.is_committable();
            ui.add_enabled_ui(can, |ui| {
                let label = if st.editing.is_some() {
                    "Update pivot"
                } else {
                    "Create pivot"
                };
                if ui.button(RichText::new(label).strong()).clicked() {
                    outcome = PivotBuilderOutcome::Commit;
                }
            });
            if !can {
                ui.label(
                    RichText::new("Add at least one Row or Value.")
                        .color(th.text_dim)
                        .small(),
                );
            }
        });

    outcome
}

/// Draw the Rows well: a drop zone holding removable, reorderable chips.
fn rows_well(ui: &mut egui::Ui, st: &mut PivotBuilderState, th: Theme) {
    let mut remove: Option<usize> = None;
    let mut reorder_to: Option<(usize, usize)> = None;

    let frame = egui::Frame::none()
        .fill(th.bg)
        .inner_margin(6.0)
        .stroke(egui::Stroke::new(1.0_f32, th.grid_line));
    let (_, dropped) = ui.dnd_drop_zone::<DragPayload, _>(frame, |ui| {
        ui.set_min_height(30.0);
        ui.horizontal_wrapped(|ui| {
            if st.rows.is_empty() {
                ui.label(
                    RichText::new("drop columns here")
                        .color(th.text_dim)
                        .small(),
                );
            }
            for (i, &col) in st.rows.clone().iter().enumerate() {
                let label = st.label_of(col);
                let id = egui::Id::new(("ferrix_pivot_row", i, col.0));
                let resp = ui
                    .dnd_drag_source(id, col, |ui| {
                        if chip(ui, th, &label, Some("✕")) {
                            remove = Some(i);
                        }
                    })
                    .response;
                // Dropping one row chip onto another reorders within the well.
                if let Some(payload) = resp.dnd_release_payload::<DragPayload>() {
                    if let Some(from) = st.rows.iter().position(|c| *c == *payload) {
                        reorder_to = Some((from, i));
                    }
                }
            }
        });
    });

    if let Some((from, to)) = reorder_to {
        st.reorder_row(from, to);
    } else if let Some(col) = dropped {
        st.drop_row(*col);
    }
    if let Some(i) = remove {
        st.remove_row(i);
    }
}

/// Draw the Values well: chips with an aggregate dropdown, a move-up control
/// (reorder within the well) and a remove button.
fn values_well(ui: &mut egui::Ui, st: &mut PivotBuilderState, th: Theme) {
    let mut remove: Option<usize> = None;
    let mut set_agg: Option<(usize, PivotAgg)> = None;
    let mut move_up: Option<usize> = None;

    let frame = egui::Frame::none()
        .fill(th.bg)
        .inner_margin(6.0)
        .stroke(egui::Stroke::new(1.0_f32, th.grid_line));
    let (_, dropped) = ui.dnd_drop_zone::<DragPayload, _>(frame, |ui| {
        ui.set_min_height(30.0);
        if st.values.is_empty() {
            ui.label(
                RichText::new("drop columns here")
                    .color(th.text_dim)
                    .small(),
            );
        }
        for (i, field) in st.values.clone().iter().enumerate() {
            let label = st.label_of(field.col);
            ui.horizontal(|ui| {
                // Move this field one place earlier. A value well can hold the
                // same column twice (Sum and Avg of it), so a drag payload of
                // ColIdx would be ambiguous here — an explicit control is the
                // unambiguous way to reorder these.
                if ui
                    .add_enabled(i > 0, egui::Button::new("↑").frame(false))
                    .on_hover_text("Move up")
                    .clicked()
                {
                    move_up = Some(i);
                }
                ui.label(RichText::new(&label).color(th.text));
                egui::ComboBox::from_id_salt(("ferrix_pivot_val_agg", i))
                    .selected_text(agg_label(field.agg))
                    .show_ui(ui, |ui| {
                        for (agg, name) in AGGS {
                            if ui.selectable_label(field.agg == agg, name).clicked() {
                                set_agg = Some((i, agg));
                            }
                        }
                    });
                if ui.button("✕").on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
            });
        }
    });

    if let Some(col) = dropped {
        st.drop_value(*col, st.default_agg);
    }
    if let Some(i) = move_up {
        st.reorder_value(i, i - 1);
    }
    if let Some((i, agg)) = set_agg {
        st.set_value_agg(i, agg);
    }
    if let Some(i) = remove {
        st.remove_value(i);
    }
}

/// A pill-shaped column chip. Returns whether its trailing button was clicked.
fn chip(ui: &mut egui::Ui, th: Theme, label: &str, trailing: Option<&str>) -> bool {
    let mut clicked = false;
    egui::Frame::none()
        .fill(th.accent_soft)
        .inner_margin(egui::Margin::symmetric(6.0, 2.0))
        .rounding(6.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).color(th.text));
                if let Some(t) = trailing {
                    if ui
                        .add(egui::Button::new(t).frame(false))
                        .on_hover_text("Remove")
                        .clicked()
                    {
                        clicked = true;
                    }
                }
            });
        });
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::{ColIdx, SheetId};

    fn state() -> PivotBuilderState {
        PivotBuilderState::new(
            SheetId(0),
            vec![
                (ColIdx(0), "region".into()),
                (ColIdx(1), "amount".into()),
                (ColIdx(2), "qty".into()),
            ],
            None,
        )
    }

    #[test]
    fn dropping_a_row_twice_keeps_one() {
        let mut st = state();
        st.drop_row(ColIdx(0));
        st.drop_row(ColIdx(0));
        assert_eq!(st.rows, vec![ColIdx(0)], "a group column is not duplicated");
    }

    #[test]
    fn values_allow_the_same_column_with_different_aggs() {
        let mut st = state();
        st.drop_value(ColIdx(1), PivotAgg::Sum);
        st.drop_value(ColIdx(1), PivotAgg::Avg);
        assert_eq!(
            st.values,
            vec![
                ValueField {
                    col: ColIdx(1),
                    agg: PivotAgg::Sum
                },
                ValueField {
                    col: ColIdx(1),
                    agg: PivotAgg::Avg
                },
            ],
            "Sum and Avg of one column are two distinct fields"
        );
    }

    #[test]
    fn reorder_moves_a_row_within_the_well() {
        let mut st = state();
        st.drop_row(ColIdx(0));
        st.drop_row(ColIdx(1));
        st.drop_row(ColIdx(2));
        // Move the first (region) to the end.
        st.reorder_row(0, 2);
        assert_eq!(
            st.rows,
            vec![ColIdx(1), ColIdx(2), ColIdx(0)],
            "reorder sets the nesting order"
        );
    }

    #[test]
    fn removing_clears_the_right_entry() {
        let mut st = state();
        st.drop_value(ColIdx(0), PivotAgg::Count);
        st.drop_value(ColIdx(1), PivotAgg::Sum);
        st.remove_value(0);
        assert_eq!(st.values.len(), 1);
        assert_eq!(st.values[0].col, ColIdx(1), "the second value survived");
    }

    #[test]
    fn spec_projects_the_wells_to_the_kernel_shape() {
        let mut st = state();
        st.drop_row(ColIdx(0));
        st.drop_value(ColIdx(1), PivotAgg::Sum);
        let spec = st.spec();
        assert_eq!(spec.group_by, vec![ColIdx(0)]);
        assert_eq!(spec.values, vec![(ColIdx(1), PivotAgg::Sum)]);
    }

    #[test]
    fn committable_needs_a_row_or_a_value() {
        let mut st = state();
        assert!(!st.is_committable(), "empty is not committable");
        st.drop_row(ColIdx(0));
        assert!(st.is_committable(), "one row is enough");
    }

    #[test]
    fn seed_fills_the_wells_from_an_existing_spec() {
        let mut st = state();
        st.seed(&PivotSpec {
            group_by: vec![ColIdx(2)],
            values: vec![(ColIdx(1), PivotAgg::Max)],
        });
        assert_eq!(st.rows, vec![ColIdx(2)]);
        assert_eq!(
            st.values,
            vec![ValueField {
                col: ColIdx(1),
                agg: PivotAgg::Max
            }]
        );
    }

    #[test]
    fn every_agg_has_a_label() {
        for (agg, name) in AGGS {
            assert_eq!(agg_label(agg), name, "label round-trips");
        }
    }
}
