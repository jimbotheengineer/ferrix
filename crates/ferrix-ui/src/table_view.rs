//! Rendering support for structured tables.
//!
//! The grid paints cells; this decides what a table wants those cells to look
//! like. Keeping it separate keeps `grid.rs` about geometry and input.
//!
//! ## Why this is per-viewport, not per-table
//!
//! Everything here is computed for the ~1,500 cells actually on screen. A
//! table over 200M rows costs the same per frame as one over 200: the
//! conditional-format extent comes from the visible rows only, validation is
//! asked one cell at a time, and the filter's row mapping is a rank lookup
//! rather than a scan. Nothing in this module is allowed to touch a row that
//! is not being painted, with one exception — the uniqueness index, which
//! cannot be answered locally and is therefore built once per filter change by
//! the caller and passed in.

use egui::Color32;
use ferrix_core::{CellStyle, RowMask, Table, UniquenessIndex, Value, Violation};

use crate::sheet_view::SheetView;

/// Everything the grid needs to draw one table, resolved for the current
/// frame.
pub struct TableDecor<'a> {
    pub table: &'a Table,
    /// Rows surviving the header filters. `None` means unfiltered.
    pub mask: Option<&'a RowMask>,
    /// Per-table-column uniqueness indexes, built by the caller when a
    /// `Unique` rule is configured. Indexed by table column.
    pub uniques: &'a [Option<UniquenessIndex>],
    /// Per-table-column numeric extent over the *visible* rows, for colour
    /// scales and data bars.
    pub extents: Vec<Option<(f64, f64)>>,
}

/// How one cell should be painted.
#[derive(Clone, Debug, Default)]
pub struct CellDecor {
    /// Display text, when the column's number format overrides the default.
    pub text: Option<String>,
    /// Background fill from a conditional rule.
    pub fill: Option<Color32>,
    /// Text colour from a conditional rule.
    pub text_color: Option<Color32>,
    /// Data-bar fraction and colour.
    pub bar: Option<(f32, Color32)>,
    /// Why the cell is invalid, when it is. Drives the red corner marker.
    pub violation: Option<Violation>,
    /// True when this row falls on the table's banded stripe.
    pub banded: bool,
    /// Type styling resolved for this cell. Empty means the grid default.
    pub typography: ferrix_core::format::Typography,
}

impl CellDecor {
    pub fn is_plain(&self) -> bool {
        self.text.is_none()
            && self.fill.is_none()
            && self.text_color.is_none()
            && self.bar.is_none()
            && self.violation.is_none()
            && !self.banded
    }
}

fn to_color32(c: ferrix_core::Rgb) -> Color32 {
    Color32::from_rgb(c.0, c.1, c.2)
}

impl<'a> TableDecor<'a> {
    /// Prepare a table's decoration for the rows about to be painted.
    ///
    /// `rows` is the visible *data* row range, already mapped through the
    /// filter. The extent pass reads only those rows, which is what keeps the
    /// cost independent of the table's height.
    pub fn prepare(
        table: &'a Table,
        mask: Option<&'a RowMask>,
        uniques: &'a [Option<UniquenessIndex>],
        view: &SheetView<'_>,
        rows: std::ops::Range<u32>,
    ) -> Self {
        let mut extents = Vec::with_capacity(table.columns.len());
        for (i, col) in table.columns.iter().enumerate() {
            if col.conditional.is_empty() {
                extents.push(None);
                continue;
            }
            let sheet_col = table.sheet_col(i);
            let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
            let mut seen = false;
            for r in rows.clone() {
                if let Value::Number(n) = view.get(ferrix_core::CellRef::new(r, sheet_col)) {
                    lo = lo.min(n);
                    hi = hi.max(n);
                    seen = true;
                }
            }
            extents.push(seen.then_some((lo, hi)));
        }
        Self {
            table,
            mask,
            uniques,
            extents,
        }
    }

    /// Map a view row to the underlying data row, honouring the filter.
    ///
    /// With a filter active the grid's row `n` is the `n`th *surviving* row,
    /// which is a rank lookup rather than an index. Rows outside the table are
    /// never remapped, so a table can sit inside a larger sheet.
    pub fn data_row(&self, view_row: usize) -> Option<usize> {
        match self.mask {
            None => Some(view_row),
            Some(m) => m.nth_visible(view_row),
        }
    }

    /// Total rows the grid should offer to scroll through.
    pub fn visible_row_count(&self, unfiltered: usize) -> usize {
        match self.mask {
            None => unfiltered,
            Some(m) => m.visible_rows(),
        }
    }

    /// Resolve one cell's decoration. Cheap enough to call per painted cell.
    pub fn cell(&self, view: &SheetView<'_>, cell: ferrix_core::CellRef) -> CellDecor {
        let mut d = CellDecor::default();
        let Some(i) = self.table.column_index(cell.col) else {
            return d;
        };
        if !self.table.contains_data(cell) {
            return d;
        }
        let Some(col) = self.table.columns.get(i) else {
            return d;
        };
        d.banded = self.table.is_banded(cell.row);

        let value = view.get(cell);

        // Number format. A format Ferrix does not model renders as the plain
        // number — a cosmetic fallback, never a change to the stored value.
        if let Value::Number(n) = value {
            if col.format != ferrix_core::NumberFormat::General {
                d.text = Some(col.format.render(n));
            }
        }

        // Conditional formatting, applied in order so a later rule wins —
        // matching Excel's own precedence.
        if !col.conditional.is_empty() {
            if let Value::Number(n) = value {
                let mut style = CellStyle::default();
                for rule in &col.conditional {
                    rule.apply(n, self.extents[i], &mut style);
                }
                d.fill = style.fill.map(to_color32);
                d.text_color = style.text.map(to_color32);
                d.bar = style.bar.map(|(f, c)| (f, to_color32(c)));
            }
        }

        // Validation. Resolving the display text is the only string cost, so
        // it is paid only for the rules that actually need it.
        if !col.validation.is_vacuous() || col.ctype != ferrix_core::ColumnType::Any {
            let text = match &col.validation.rule {
                ferrix_core::ValidationRule::OneOf(_)
                | ferrix_core::ValidationRule::Regex(_)
                | ferrix_core::ValidationRule::TextLength { .. } => view.display(cell),
                _ => String::new(),
            };
            d.violation = self.table.validate_cell(
                i,
                &value,
                &text,
                self.uniques.get(i).and_then(|u| u.as_ref()),
            );
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::{
        ColumnType, ConditionalRule, EditOverlay, NumberFormat, Rgb, Sheet, TableColumn,
        TableRange, Validation, ValidationRule,
    };

    use crate::sheet_view::BaseData;

    fn fixture() -> (BaseData, EditOverlay, Table) {
        let mut s = Sheet::new("s");
        s.set_text(ferrix_core::CellRef::new(0, 0), "amount");
        for r in 1..=10u32 {
            s.set(ferrix_core::CellRef::new(r, 0), Value::Number(r as f64));
        }
        let t = Table::new("T", TableRange::new(0, 0, 10, 0)).with_columns(vec![TableColumn::new(
            "amount",
        )
        .typed(ColumnType::Number)
        .formatted(NumberFormat::Currency {
            symbol: "$".into(),
            places: 2,
        })
        .with_conditional(ConditionalRule::ColorScale2 {
            min: Rgb(0, 0, 0),
            max: Rgb(100, 100, 100),
        })
        .validated(Validation::new(ValidationRule::Between {
            min: 0.0,
            max: 8.0,
        }))]);
        (BaseData::Memory(s), EditOverlay::new(), t)
    }

    #[test]
    fn number_format_reaches_the_grid() {
        let (base, ov, t) = fixture();
        let view = SheetView::new(&base, &ov);
        let d = TableDecor::prepare(&t, None, &[], &view, 1..11);
        let c = d.cell(&view, ferrix_core::CellRef::new(1, 0));
        assert_eq!(c.text.as_deref(), Some("$1.00"), "format not applied");
    }

    #[test]
    fn the_header_row_is_not_decorated() {
        let (base, ov, t) = fixture();
        let view = SheetView::new(&base, &ov);
        let d = TableDecor::prepare(&t, None, &[], &view, 1..11);
        assert!(d.cell(&view, ferrix_core::CellRef::new(0, 0)).is_plain());
    }

    #[test]
    fn conditional_fill_scales_over_the_visible_rows_only() {
        let (base, ov, t) = fixture();
        let view = SheetView::new(&base, &ov);
        // Only rows 1..=5 visible: the extent is 1..5, so row 5 is the max.
        let d = TableDecor::prepare(&t, None, &[], &view, 1..6);
        assert_eq!(d.extents[0], Some((1.0, 5.0)));
        let top = d.cell(&view, ferrix_core::CellRef::new(5, 0));
        assert_eq!(top.fill, Some(Color32::from_rgb(100, 100, 100)));
        let bottom = d.cell(&view, ferrix_core::CellRef::new(1, 0));
        assert_eq!(bottom.fill, Some(Color32::from_rgb(0, 0, 0)));
    }

    #[test]
    fn invalid_cells_are_flagged_for_the_painter() {
        let (base, ov, t) = fixture();
        let view = SheetView::new(&base, &ov);
        let d = TableDecor::prepare(&t, None, &[], &view, 1..11);
        // Rows 9 and 10 exceed the max of 8.
        assert!(d
            .cell(&view, ferrix_core::CellRef::new(8, 0))
            .violation
            .is_none());
        let bad = d.cell(&view, ferrix_core::CellRef::new(9, 0));
        assert!(
            matches!(bad.violation, Some(Violation::OutOfRange { .. })),
            "a bad cell must be visibly flagged, got {:?}",
            bad.violation
        );
        // ...and the value itself is untouched.
        assert_eq!(
            view.get(ferrix_core::CellRef::new(9, 0)),
            Value::Number(9.0)
        );
    }

    #[test]
    fn banding_alternates_within_the_table() {
        let (base, ov, t) = fixture();
        let view = SheetView::new(&base, &ov);
        let d = TableDecor::prepare(&t, None, &[], &view, 1..11);
        assert!(!d.cell(&view, ferrix_core::CellRef::new(1, 0)).banded);
        assert!(d.cell(&view, ferrix_core::CellRef::new(2, 0)).banded);
        assert!(!d.cell(&view, ferrix_core::CellRef::new(3, 0)).banded);
    }

    #[test]
    fn a_filter_remaps_view_rows_to_data_rows() {
        let (base, ov, mut t) = fixture();
        t.columns[0].filter = Some(ferrix_core::Predicate::Compare {
            op: ferrix_core::CmpOp::Gt,
            value: 7.0,
        });
        let BaseData::Memory(sheet) = &base else {
            unreachable!()
        };
        let mask = sheet.filter_table(&t, usize::MAX);
        let view = SheetView::new(&base, &ov);
        let d = TableDecor::prepare(&t, Some(&mask), &[], &view, 8..11);

        // Header (row 0) plus rows 8, 9, 10.
        assert_eq!(d.visible_row_count(11), 4);
        assert_eq!(d.data_row(0), Some(0), "the header stays put");
        assert_eq!(d.data_row(1), Some(8));
        assert_eq!(d.data_row(3), Some(10));
        assert_eq!(d.data_row(4), None, "past the end of the filtered view");
    }

    #[test]
    fn cells_outside_the_table_are_untouched() {
        let (base, ov, t) = fixture();
        let view = SheetView::new(&base, &ov);
        let d = TableDecor::prepare(&t, None, &[], &view, 1..11);
        assert!(d.cell(&view, ferrix_core::CellRef::new(5, 9)).is_plain());
        assert!(d.cell(&view, ferrix_core::CellRef::new(99, 0)).is_plain());
    }
}
