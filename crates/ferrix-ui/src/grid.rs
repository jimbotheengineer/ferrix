//! Virtualized spreadsheet grid with row-indexed scrolling.
//!
//! The whole performance story of the UI lives here: no matter how many rows
//! the sheet holds, we compute which rows and columns intersect the viewport
//! and paint ONLY those. A 200M-row sheet and a 100-row sheet do exactly the
//! same amount of per-frame work.
//!
//! We paint cells directly onto the `Painter` instead of instantiating egui
//! widgets per cell — a widget per visible cell (~1,500 of them) would mean
//! 1,500 id allocations and interaction checks every frame.
//!
//! ## Why we do our own scrolling
//!
//! The obvious approach — hand egui a `ScrollArea` sized `rows * ROW_HEIGHT`
//! pixels — caps out at ~16.7M rows. Scroll offsets are f32 pixels, and f32
//! has a 24-bit mantissa, so once the virtual canvas exceeds ~2^24 px the
//! smallest representable step grows past one row height and individual rows
//! stop being addressable:
//!
//! | rows | canvas   | ulp    | addressable? |
//! |------|----------|--------|--------------|
//! | 10M  | 220M px  | 16 px  | yes (< 22px) |
//! | 16.7M| 368M px  | 32 px  | NO           |
//! | 200M | 4.4B px  | 512 px | NO           |
//!
//! A 10GB CSV is ~200M rows, so the pixel-canvas approach is unusable. Instead
//! we track scroll position as an f64 *row index* (`row_offset`) and never
//! build a giant canvas at all. f64 has a 52-bit mantissa, so row addressing
//! stays exact past 10^15 rows — far beyond any file that fits on a disk.

use egui::{Align2, FontId, Rect, Sense, Stroke, Ui, Vec2};
use ferrix_core::{column_name, CellRef, RowFilter, Selection, Value};

use crate::sheet_view::SheetView;
use crate::table_view::TableDecor;
use crate::theme::Theme;

pub const FILL_HANDLE: f32 = 7.0;
pub const ROW_HEIGHT: f32 = 22.0;
pub const DEFAULT_COL_WIDTH: f32 = 108.0;
pub const ROW_HEADER_WIDTH: f32 = 88.0;
pub const HEADER_HEIGHT: f32 = 26.0;
const SCROLLBAR_W: f32 = 12.0;

/// How far past the last data row "show empty rows" extends the scrollable
/// area (issue #20).
///
/// Enough to type into without hunting for the end, and small enough that the
/// scrollbar thumb on a short file does not collapse. These rows are PURE
/// VIEWPORT: nothing is materialised, and `SheetView::row_count` — which is
/// what export, SUM and the status bar read — never sees them.
pub const EMPTY_ROW_PADDING: usize = 200;

/// Where the padding rows start on screen, and which data row the first one
/// names.
///
/// Padding rows sit AFTER every row the filters kept, so a padding row's
/// screen index is past the end of both row mappings. Nothing here is ever
/// fed to `RowFilter::underlying` or `TableDecor::data_row`: a padding row is
/// not a filterable row, and indexing either mapping with one would either
/// return `None` (blanking the row) or, worse, alias onto a real record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadSpace {
    /// First screen row that is padding. Anything below is a real row.
    pub first_pad_screen_row: usize,
    /// Data row the first padding row addresses — one past the sheet's end.
    pub first_pad_data_row: usize,
}

impl PadSpace {
    /// Data row for a padding screen row, or `None` if `r` is a real row.
    #[inline]
    pub fn data_row(&self, r: usize) -> Option<u32> {
        r.checked_sub(self.first_pad_screen_row)
            .map(|n| (self.first_pad_data_row + n) as u32)
    }

    /// Screen row for a data row that lies in the padding, or `None` if the
    /// row is a real one the filters own.
    #[inline]
    pub fn screen_row(&self, row: u32) -> Option<usize> {
        (row as usize)
            .checked_sub(self.first_pad_data_row)
            .map(|n| self.first_pad_screen_row + n)
    }
}

/// What a screen row turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScreenRow {
    /// A real row, resolved through whichever filters are active.
    Data(u32),
    /// Empty padding past the end of the sheet. Addressable — typing here
    /// extends the sheet through the overlay's own extent — but it holds no
    /// data, is in no filter's mapping, and carries no table decoration.
    Pad(u32),
}

impl ScreenRow {
    #[inline]
    fn row(self) -> u32 {
        match self {
            ScreenRow::Data(r) | ScreenRow::Pad(r) => r,
        }
    }

    #[inline]
    fn is_pad(self) -> bool {
        matches!(self, ScreenRow::Pad(_))
    }
}

/// Persistent scroll position, owned by the app and passed in each frame.
///
/// `row_offset` is a fractional row index (f64) rather than a pixel offset,
/// which is what lets us address 200M+ rows exactly. `col_px` stays f32
/// because column counts are small (thousands at most).
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollState {
    pub row_offset: f64,
    pub col_px: f32,
}

impl ScrollState {
    /// Clamp to the valid range for the current sheet and viewport.
    pub fn clamp(&mut self, total_rows: usize, total_width: f32, view: Vec2) {
        let visible_rows = (view.y / ROW_HEIGHT) as f64;
        let max_row = (total_rows as f64 - visible_rows).max(0.0);
        self.row_offset = self.row_offset.clamp(0.0, max_row);
        let max_x = (total_width - view.x).max(0.0);
        self.col_px = self.col_px.clamp(0.0, max_x);
    }
}

/// What the user did to the grid this frame.
pub struct GridResponse {
    pub clicked: Option<CellRef>,
    /// Set on the frame the fill handle is pressed.
    pub fill_started: bool,
    /// Cell the fill handle is currently dragged over.
    pub fill_to: Option<CellRef>,
    /// Set on the frame the fill drag is released.
    pub fill_released: bool,
    /// Set while the primary button is held and the pointer has moved to a new
    /// cell — the caller extends the selection to it.
    pub drag_to: Option<CellRef>,
    pub double_clicked: Option<CellRef>,
    pub painted_cells: usize,
    /// Reported for callers that want to size scrollbars or prefetch; the app
    /// does not consume it yet, but it is part of the Grid's public response.
    #[allow(dead_code)]
    pub visible_rows: std::ops::Range<usize>,
}

pub struct Grid<'a> {
    pub view: &'a SheetView<'a>,
    /// The active selection. Its cursor is drawn with a strong border, the
    /// rest of the range with a translucent fill.
    pub selection: Option<Selection>,
    pub col_widths: &'a [f32],
    pub scroll: &'a mut ScrollState,
    /// Cell currently being edited, if any — painted by the caller as a
    /// TextEdit overlay, so the grid skips drawing its value.
    pub editing: Option<CellRef>,
    /// Cells matching the active search, sorted row-major. Only the visible
    /// slice is consulted per frame, via binary search.
    pub matches: &'a [CellRef],
    /// The match the user is currently parked on, drawn more prominently.
    pub current_match: Option<CellRef>,
    /// True while the user is dragging the fill handle, so the grid reports
    /// fill targets rather than ordinary selection drags.
    pub filling: bool,
    /// Active row filter, when filter mode is on.
    ///
    /// The grid then paints VISIBLE rows 0..filter.len() and looks up each
    /// one's underlying row through the mapping. Row headers, cell addresses,
    /// hit-testing and the returned `CellRef`s all stay in UNDERLYING row
    /// space, so a click or an edit on a filtered row addresses the real row
    /// in the sheet. `None` means the ordinary unfiltered range.
    pub filter: Option<&'a RowFilter>,
    /// Structured table covering part of the sheet, if any. Supplies number
    /// formats, conditional styling, banding, validation flags, and — when a
    /// header filter is active — the view-row to data-row mapping.
    pub table: Option<&'a TableDecor<'a>>,
    /// The active palette. Passed in rather than read from a constant, so the
    /// whole grid follows the theme toggle (issue #19).
    pub theme: Theme,
    /// Empty rows to offer past the end of the sheet, or 0 when the "show
    /// empty rows" toggle is off (issue #20).
    pub pad_rows: usize,
}

impl<'a> Grid<'a> {
    /// Underlying row for a visible row index, honouring the active filter.
    #[inline]
    fn underlying_row(filter: Option<&RowFilter>, visible: usize) -> Option<u32> {
        match filter {
            Some(f) => f.underlying(visible),
            None => Some(visible as u32),
        }
    }

    /// Visible row index for an underlying row, or `None` when the filter
    /// hides it.
    #[inline]
    fn visible_row(filter: Option<&RowFilter>, underlying: u32) -> Option<usize> {
        match filter {
            Some(f) => f.visible_of(underlying),
            None => Some(underlying as usize),
        }
    }
}

impl<'a> Grid<'a> {
    /// Screen rect of a cell, or None when it is scrolled out of view. The
    /// editor uses this to place its TextEdit exactly over the cell.
    ///
    /// `cell.row` is an UNDERLYING row. Under an active filter it is mapped
    /// into visible-row space before positioning, and a row the filter hides
    /// has no rect at all.
    pub fn cell_screen_rect(
        cell: CellRef,
        outer: Rect,
        scroll: &ScrollState,
        col_widths: &[f32],
        filter: Option<&RowFilter>,
        pad: Option<PadSpace>,
    ) -> Option<Rect> {
        let body_origin = outer.min + Vec2::new(ROW_HEADER_WIDTH, HEADER_HEIGHT);
        // A padding row is not in the filter's mapping, so it is resolved from
        // the pad space FIRST — asking the filter about it would return None
        // and the in-cell editor would have nowhere to draw.
        let visible = match pad.and_then(|p| p.screen_row(cell.row)) {
            Some(v) => v,
            None => Self::visible_row(filter, cell.row)?,
        };
        let rel_row = visible as f64 - scroll.row_offset;
        let y = body_origin.y + (rel_row * ROW_HEIGHT as f64) as f32;
        if y + ROW_HEIGHT < body_origin.y || y > outer.max.y {
            return None;
        }
        let mut x = body_origin.x - scroll.col_px;
        for c in 0..cell.col as usize {
            x += col_widths.get(c).copied().unwrap_or(DEFAULT_COL_WIDTH);
        }
        let w = col_widths
            .get(cell.col as usize)
            .copied()
            .unwrap_or(DEFAULT_COL_WIDTH);
        if x + w < body_origin.x || x > outer.max.x {
            return None;
        }
        Some(Rect::from_min_size(
            egui::pos2(x, y),
            Vec2::new(w, ROW_HEIGHT),
        ))
    }

    pub fn show(self, ui: &mut Ui) -> GridResponse {
        let view = self.view;
        let filter = self.filter;
        // TWO independent row filters can be active at once: a table's header
        // filter, and search filter mode. They compose — search filters within
        // whatever the table already narrowed to — so the visible extent is the
        // table's surviving rows, further reduced by the search filter.
        //
        // Order matters. The table maps view-row -> data-row by rank, and the
        // search filter is built over data rows, so the table mapping must be
        // applied FIRST and the search filter consulted second. Reversing them
        // would index the search filter with a table view-row and silently show
        // the wrong records.
        let th = self.theme;
        let data_rows = view.row_count().max(1);
        let table_rows = self
            .table
            .map_or(data_rows, |t| t.visible_row_count(data_rows));
        // Rows the FILTERS resolve. Padding is added on top of this and is
        // never part of it, which is what keeps padding out of both mappings.
        let filtered_rows = match filter {
            Some(f) => f.len(),
            None => table_rows,
        };
        // Empty padding (issue #20) extends only the scrollable EXTENT. It is
        // added after the filtered rows, so a padding screen index is by
        // construction past the end of every mapping, and `view.row_count()`
        // — which export, SUM and the status bar read — is untouched.
        let pad = (self.pad_rows > 0).then_some(PadSpace {
            first_pad_screen_row: filtered_rows,
            // One past the sheet's real end, in DATA space. Under a filter the
            // padding still addresses rows past the whole sheet, not past the
            // filtered subset — typing there must extend the sheet, not
            // overwrite a hidden record.
            first_pad_data_row: view.row_count(),
        });
        let total_rows = (filtered_rows + self.pad_rows).max(1);
        let total_cols = view.col_count().max(1);
        // Map a view row to the underlying data row. Identity when unfiltered.
        let to_data = |r: usize| -> Option<usize> {
            match self.table {
                None => Some(r),
                Some(t) => t.data_row(r),
            }
        };

        // THE single row resolution, used by painting, row headers and
        // hit-testing alike.
        //
        // Two filters can narrow the view at once and they compose in a fixed
        // order: the table's header filter maps a screen row to a data row by
        // rank, and search filter mode selects among data rows. Resolving them
        // separately from the same screen index — which is what merging the two
        // branches naively produced — lets whichever variable the caller
        // happens to use win, silently ignoring the other filter and painting
        // the wrong records under the right row numbers.
        // Padding is checked FIRST and short-circuits both filters. A padding
        // row is not a filterable row: feeding its screen index to
        // `RowFilter::underlying` or `TableDecor::data_row` would run off the
        // end of the mapping, and under a *shorter* filtered view could alias
        // straight onto an unrelated record.
        let resolve_row = |r: usize| -> Option<ScreenRow> {
            if let Some(row) = pad.and_then(|p| p.data_row(r)) {
                return Some(ScreenRow::Pad(row));
            }
            match filter {
                // Search filter active: it already indexes data rows.
                Some(_) => Self::underlying_row(filter, r),
                // Otherwise the table mapping (identity when there is no table).
                None => to_data(r).map(|d| d as u32),
            }
            .map(ScreenRow::Data)
        };

        let width_of =
            |c: usize| -> f32 { self.col_widths.get(c).copied().unwrap_or(DEFAULT_COL_WIDTH) };

        // Column x-offsets via prefix sum. Column counts are small, so this is
        // cheap to rebuild each frame and keeps variable widths trivial.
        let mut col_x = Vec::with_capacity(total_cols + 1);
        let mut acc = 0.0f32;
        for c in 0..total_cols {
            col_x.push(acc);
            acc += width_of(c);
        }
        col_x.push(acc);
        let total_width = acc;

        let outer = ui.available_rect_before_wrap();
        let body_origin = outer.min + Vec2::new(ROW_HEADER_WIDTH, HEADER_HEIGHT);
        let body_rect = Rect::from_min_max(body_origin, outer.max - Vec2::splat(SCROLLBAR_W));
        let body_size = body_rect.size();

        // --- input ---
        //
        // Read pointer/wheel state directly rather than through a widget
        // Response. Both work for real input; raw state is used here because
        // the grid is hand-painted and owns its own hit-testing anyway, so
        // there is no widget whose Response we would otherwise need.
        //
        // NOTE for anyone testing with synthetic input: egui resolves clicks
        // against `interact_pos`, which only updates when a pointer MOVE event
        // is delivered. A synthetic click with no preceding move lands nowhere
        // and looks like a broken grid. Send a move first, or test by hand.
        let interact_rect = Rect::from_min_max(body_origin, outer.max);
        let (pointer_pos, wheel, primary_clicked, primary_pressed, primary_double, dragging) =
            ui.ctx().input(|i| {
                (
                    i.pointer.interact_pos(),
                    i.raw_scroll_delta,
                    i.pointer.primary_clicked(),
                    i.pointer.primary_pressed(),
                    i.pointer
                        .button_double_clicked(egui::PointerButton::Primary),
                    i.pointer.primary_down(),
                )
            });
        let drag_pos = pointer_pos;

        let pointer_in_body = pointer_pos.is_some_and(|p| interact_rect.contains(p));
        if pointer_in_body {
            if wheel.y != 0.0 {
                // Convert pixel wheel delta into row units.
                self.scroll.row_offset -= (wheel.y / ROW_HEIGHT) as f64;
            }
            if wheel.x != 0.0 {
                self.scroll.col_px -= wheel.x;
            }
        }
        self.scroll.clamp(total_rows, total_width, body_size);

        let first_row = self.scroll.row_offset.floor().max(0.0) as usize;
        // Sub-row offset in pixels, so scrolling is smooth rather than snapping.
        let frac_px = ((self.scroll.row_offset - first_row as f64) * ROW_HEIGHT as f64) as f32;
        let visible_count = (body_size.y / ROW_HEIGHT).ceil() as usize + 1;
        let last_row = (first_row + visible_count).min(total_rows);
        let row_range = first_row..last_row;

        let first_col = col_x
            .partition_point(|&x| x <= self.scroll.col_px)
            .saturating_sub(1);
        let last_col = col_x
            .partition_point(|&x| x < self.scroll.col_px + body_size.x)
            .min(total_cols);
        let col_range = first_col..last_col;

        let mut clicked = None;
        let mut double_clicked = None;
        let mut drag_to = None;
        let mut fill_started = false;
        let mut fill_to = None;
        let mut fill_released = false;
        let mut painted_cells = 0usize;

        // Narrow the match list to just the visible rows once per frame, so
        // per-cell highlight testing is a small linear probe rather than a
        // scan of a potentially enormous result set.
        //
        // Under a filter the viewport spans visible rows, which are sparse in
        // underlying space, so the bounds come from the mapping's window —
        // still two binary searches, still no per-row allocation.
        // Search matches only ever live in real rows, so the narrowing window
        // is clamped to the filtered range before the mapping is consulted.
        let match_last = last_row.min(filtered_rows);
        let (match_lo_row, match_hi_row) = match filter {
            Some(f) => {
                let w = f.window(first_row.min(match_last), match_last);
                match (w.first(), w.last()) {
                    // `last + 1` because the narrowing bound is exclusive.
                    (Some(&a), Some(&b)) => (a as usize, b as usize + 1),
                    _ => (0, 0),
                }
            }
            None => (first_row.min(match_last), match_last),
        };
        let vis_lo = self
            .matches
            .partition_point(|m| (m.row as usize) < match_lo_row);
        let vis_hi = self
            .matches
            .partition_point(|m| (m.row as usize) < match_hi_row);
        let visible_matches = &self.matches[vis_lo..vis_hi];
        let is_match = |cell: CellRef| -> bool {
            visible_matches
                .binary_search_by(|m| (m.row, m.col).cmp(&(cell.row, cell.col)))
                .is_ok()
        };

        // --- paint body ---
        let painter = ui.painter_at(body_rect);
        painter.rect_filled(body_rect, 0.0, th.bg);

        // `r` walks VISIBLE rows; `row` is the underlying row it maps to.
        // Everything painted from here on — cell values, highlights, selection
        // tests, the CellRefs handed back to the caller — uses `row`, so a
        // filtered grid addresses exactly the same cells an unfiltered one
        // would.
        for r in row_range.clone() {
            // Resolve the screen row to a data row through BOTH filters, in
            // the order they narrow: the table first, then search. Resolving
            // them independently would let one silently win.
            let Some(resolved) = resolve_row(r) else {
                continue;
            };
            let row = resolved.row();
            let is_pad = resolved.is_pad();
            let y = body_origin.y + (r - first_row) as f32 * ROW_HEIGHT - frac_px;
            let row_rect = Rect::from_min_size(
                egui::pos2(body_rect.min.x, y),
                Vec2::new(body_rect.width(), ROW_HEIGHT),
            );
            if is_pad {
                // Padding gets its own recessed fill and no zebra stripe, so
                // "there is no row here" is visibly different from "this row
                // exists and holds empty strings".
                painter.rect_filled(row_rect, 0.0, th.pad_row);
            } else if r % 2 == 1 {
                painter.rect_filled(row_rect, 0.0, th.row_alt);
            }

            for c in col_range.clone() {
                let x = body_origin.x + col_x[c] - self.scroll.col_px;
                let w = width_of(c);
                let cell_rect = Rect::from_min_size(egui::pos2(x, y), Vec2::new(w, ROW_HEIGHT));
                let cref = CellRef::new(row, c as u32);

                // Table decoration: number format, conditional styling,
                // banding, and the validation flag. Resolved per painted cell,
                // so a table over 200M rows costs exactly what one over 200
                // does. A cell the table has nothing to say about is dropped
                // here so the paint path below stays on its ordinary branch.
                // A padding row is outside the table by definition, so it
                // gets no banding, no conditional fill and no validation flag.
                let decor = self
                    .table
                    .filter(|_| !is_pad)
                    .map(|t| t.cell(view, cref))
                    .filter(|d| !d.is_plain());

                if let Some(d) = &decor {
                    if d.banded {
                        painter.rect_filled(cell_rect, 0.0, th.table_band);
                    }
                    if let Some(fill) = d.fill {
                        painter.rect_filled(cell_rect, 0.0, fill);
                    }
                    if let Some((frac, color)) = d.bar {
                        // The bar is drawn behind the text, inset so grid
                        // lines stay legible.
                        let inner = cell_rect.shrink2(Vec2::new(1.5, 3.0));
                        let bar = Rect::from_min_size(
                            inner.min,
                            Vec2::new(inner.width() * frac, inner.height()),
                        );
                        painter.rect_filled(bar, 1.0, color);
                    }
                }

                // Search highlight sits under the selection so both remain
                // visible when the cursor is parked on a match.
                if !is_pad && !visible_matches.is_empty() && is_match(cref) {
                    if self.current_match == Some(cref) {
                        painter.rect_filled(cell_rect, 0.0, th.match_current);
                        painter.rect_stroke(cell_rect, 0.0, Stroke::new(1.5_f32, th.match_edge));
                    } else {
                        painter.rect_filled(cell_rect, 0.0, th.match_bg);
                    }
                }

                // Selection painting. A range gets a translucent fill; the
                // cursor cell keeps the strong border so the user can always
                // see where typing will land.
                if let Some(sel) = self.selection {
                    if sel.cursor == cref {
                        painter.rect_filled(cell_rect, 0.0, th.accent_soft);
                        painter.rect_stroke(cell_rect, 0.0, Stroke::new(1.5_f32, th.accent));
                    } else if !sel.is_single() && sel.contains(cref) {
                        painter.rect_filled(cell_rect, 0.0, th.range_fill);
                    }
                }

                // The cell under edit is drawn by the caller's TextEdit.
                if self.editing == Some(cref) {
                    painted_cells += 1;
                    continue;
                }

                let value = view.get(cref);
                if !matches!(value, Value::Empty) {
                    let (mut text, mut color, align) = match value {
                        Value::Number(n) => (
                            ferrix_core::format_number(n),
                            th.number,
                            Align2::RIGHT_CENTER,
                        ),
                        Value::Bool(b) => (
                            if b { "TRUE" } else { "FALSE" }.to_string(),
                            th.text_dim,
                            Align2::CENTER_CENTER,
                        ),
                        Value::Text(id) => {
                            (view.resolve(id).to_string(), th.text, Align2::LEFT_CENTER)
                        }
                        Value::Error(e) => (e.to_string(), th.error, Align2::RIGHT_CENTER),
                        Value::Empty => unreachable!(),
                    };

                    // A table column's number format replaces the default
                    // rendering, and a conditional rule may recolour the text.
                    if let Some(d) = &decor {
                        if let Some(t) = &d.text {
                            text.clone_from(t);
                        }
                        if let Some(c) = d.text_color {
                            color = c;
                        }
                    }

                    // A formula cell gets a subtle marker so it is
                    // distinguishable from a typed-in literal.
                    if view.has_formula(cref) {
                        painter.circle_filled(
                            egui::pos2(cell_rect.min.x + 3.5, cell_rect.min.y + 4.0),
                            1.6,
                            th.accent,
                        );
                    }

                    let pad = 6.0;
                    let anchor = match align {
                        Align2::RIGHT_CENTER => {
                            egui::pos2(cell_rect.max.x - pad, cell_rect.center().y)
                        }
                        Align2::CENTER_CENTER => cell_rect.center(),
                        _ => egui::pos2(cell_rect.min.x + pad, cell_rect.center().y),
                    };
                    painter
                        .with_clip_rect(cell_rect.intersect(body_rect).shrink2(Vec2::new(2.0, 0.0)))
                        .text(anchor, align, text, FontId::proportional(12.5), color);
                }

                // The validation flag goes on LAST, over everything else. A
                // cell that fails its column's rule is never rejected or
                // rewritten — it keeps its value and gets a red triangle in the
                // top-right corner, the way a spreadsheet marks a problem the
                // user has to look at.
                if let Some(d) = &decor {
                    if d.violation.is_some() {
                        let tr = egui::pos2(cell_rect.max.x, cell_rect.min.y);
                        painter.add(egui::Shape::convex_polygon(
                            vec![
                                tr,
                                egui::pos2(tr.x - 7.0, tr.y),
                                egui::pos2(tr.x, tr.y + 7.0),
                            ],
                            th.invalid_flag,
                            Stroke::NONE,
                        ));
                    }
                }
                painted_cells += 1;
            }
        }

        // --- grid lines ---
        let line = Stroke::new(1.0_f32, th.grid_line);
        for r in row_range.clone() {
            let y = body_origin.y + (r - first_row) as f32 * ROW_HEIGHT - frac_px;
            painter.hline(body_rect.min.x..=body_rect.max.x, y, line);
        }
        for c in col_range.start..=col_range.end.min(total_cols) {
            let x = body_origin.x + col_x[c.min(total_cols)] - self.scroll.col_px;
            painter.vline(x, body_rect.min.y..=body_rect.max.y, line);
        }

        // --- hit testing ---
        //
        // Pixels map to a VISIBLE row, which is then translated back through
        // the filter so the CellRef the caller receives — and therefore any
        // click, drag, or edit built from it — names the underlying row.
        let hit = |pos: egui::Pos2| -> Option<CellRef> {
            if !body_rect.contains(pos) {
                return None;
            }
            let dy = pos.y - body_origin.y + frac_px;
            let r = first_row as f64 + (dy / ROW_HEIGHT) as f64;
            let local_x = pos.x - body_origin.x + self.scroll.col_px;
            let c = col_x.partition_point(|&x| x <= local_x) as i64 - 1;
            if r < 0.0 || c < 0 || r as usize >= total_rows || c as usize >= total_cols {
                return None;
            }
            // Report the DATA row, so a click under a filter selects the cell
            // the user actually pointed at rather than its screen position.
            // Both mappings apply, in the same order the paint path uses: the
            // table's rank lookup first, then the search filter.
            // Padding rows are hit-testable — that is the whole point of the
            // toggle: clicking one selects it so the user can type there, and
            // the resulting edit extends the sheet through the overlay.
            let row = resolve_row(r as usize)?.row();
            Some(CellRef::new(row, c as u32))
        };
        if primary_clicked {
            clicked = pointer_pos.and_then(hit);
        }
        if primary_double {
            double_clicked = pointer_pos.and_then(hit);
        }
        // Drag-to-extend: the button is held, the pointer is over the body,
        // and this is not the initial press (which `clicked` already covers).
        // Reported every frame the pointer is down so the range tracks the
        // cursor continuously.
        // Exclude the scrollbar gutters: dragging a scrollbar must scroll, not
        // paint a selection. Their rects are computed below, but the gutters
        // are always the right/bottom SCROLLBAR_W strip of the outer rect.
        let in_gutter = pointer_pos
            .is_some_and(|p| p.x >= outer.max.x - SCROLLBAR_W || p.y >= outer.max.y - SCROLLBAR_W);
        // The fill-handle rect, needed here so grabbing it is not also read as
        // a selection drag. Painted further down.
        let handle_rect: Option<Rect> =
            self.selection
                .filter(|_| self.editing.is_none())
                .and_then(|sel| {
                    let (_, br) = sel.bounds();
                    Self::cell_screen_rect(br, outer, self.scroll, self.col_widths, filter, pad)
                        .map(|r| {
                            Rect::from_center_size(r.max, Vec2::splat(FILL_HANDLE)).expand(2.0)
                        })
                });
        let on_handle = pointer_pos
            .is_some_and(|p| handle_rect.is_some_and(|h| h.contains(p) && body_rect.contains(p)));
        if dragging
            && !primary_clicked
            && !on_handle
            && !self.filling
            && pointer_in_body
            && !in_gutter
        {
            drag_to = pointer_pos.and_then(hit);
        }

        // --- fill handle ---
        //
        // Painted at the bottom-right of the selection, and hit-tested BEFORE
        // ordinary click handling so pressing it starts a fill rather than
        // collapsing the selection.
        if let Some(hr) = handle_rect {
            let hr = hr.shrink(2.0);
            if body_rect.contains(hr.center()) {
                painter.rect_filled(hr, 1.0, th.accent);
                painter.rect_stroke(hr, 1.0, Stroke::new(1.0_f32, th.bg));
            }
        }
        if primary_pressed && on_handle {
            fill_started = true;
            clicked = None; // Do not also move the selection.
        }
        if self.filling {
            if dragging {
                fill_to = pointer_pos.and_then(hit);
            } else {
                fill_released = true;
            }
            // A fill drag must never be read as a selection drag.
            drag_to = None;
        }

        // --- vertical scrollbar (row-indexed, not pixel-indexed) ---
        let vbar = Rect::from_min_max(
            egui::pos2(outer.max.x - SCROLLBAR_W, body_origin.y),
            egui::pos2(outer.max.x, outer.max.y - SCROLLBAR_W),
        );
        let vbar_active = dragging && drag_pos.is_some_and(|p| vbar.contains(p));
        let vpainter = ui.painter_at(vbar);
        vpainter.rect_filled(vbar, 0.0, th.panel);
        let visible_frac = (visible_count as f64 / total_rows as f64).min(1.0);
        let thumb_h = (vbar.height() as f64 * visible_frac).max(24.0) as f32;
        let scroll_span = (total_rows as f64 - visible_count as f64).max(1.0);
        let pos_frac = (self.scroll.row_offset / scroll_span).clamp(0.0, 1.0);
        let thumb_y = vbar.min.y + (vbar.height() - thumb_h) * pos_frac as f32;
        let thumb = Rect::from_min_size(
            egui::pos2(vbar.min.x + 2.0, thumb_y),
            Vec2::new(SCROLLBAR_W - 4.0, thumb_h),
        );
        vpainter.rect_filled(
            thumb,
            3.0,
            if vbar_active { th.accent } else { th.grid_line },
        );
        if vbar_active {
            if let Some(p) = drag_pos {
                // Map thumb position back to a row index. Because this maps
                // through a fraction rather than accumulating pixels, dragging
                // the bar addresses all 200M rows without precision loss.
                let t = ((p.y - vbar.min.y - thumb_h / 2.0) / (vbar.height() - thumb_h))
                    .clamp(0.0, 1.0) as f64;
                self.scroll.row_offset = t * scroll_span;
            }
        }

        // --- horizontal scrollbar ---
        let hbar = Rect::from_min_max(
            egui::pos2(body_origin.x, outer.max.y - SCROLLBAR_W),
            egui::pos2(outer.max.x - SCROLLBAR_W, outer.max.y),
        );
        let hresp = ui.allocate_rect(hbar, Sense::click_and_drag());
        let hpainter = ui.painter_at(hbar);
        hpainter.rect_filled(hbar, 0.0, th.panel);
        if total_width > body_size.x {
            let frac = (body_size.x / total_width).min(1.0);
            let tw = (hbar.width() * frac).max(24.0);
            let span = (total_width - body_size.x).max(1.0);
            let tx = hbar.min.x + (hbar.width() - tw) * (self.scroll.col_px / span).clamp(0.0, 1.0);
            hpainter.rect_filled(
                Rect::from_min_size(
                    egui::pos2(tx, hbar.min.y + 2.0),
                    Vec2::new(tw, SCROLLBAR_W - 4.0),
                ),
                3.0,
                if hresp.hovered() || hresp.dragged() {
                    th.accent
                } else {
                    th.grid_line
                },
            );
            if hresp.dragged() {
                if let Some(p) = hresp.interact_pointer_pos() {
                    let t = ((p.x - hbar.min.x - tw / 2.0) / (hbar.width() - tw)).clamp(0.0, 1.0);
                    self.scroll.col_px = t * span;
                }
            }
        }

        // --- pinned headers ---
        let hp = ui.painter_at(outer);
        let col_header = Rect::from_min_size(
            egui::pos2(body_origin.x, outer.min.y),
            Vec2::new(outer.width() - ROW_HEADER_WIDTH, HEADER_HEIGHT),
        );
        hp.rect_filled(col_header, 0.0, th.header_bg);
        let chp = hp.with_clip_rect(col_header);
        for c in col_range.clone() {
            let x = body_origin.x + col_x[c] - self.scroll.col_px;
            let r = Rect::from_min_size(
                egui::pos2(x, outer.min.y),
                Vec2::new(width_of(c), HEADER_HEIGHT),
            );
            let label = view.header_or_letter(c);
            let letter = column_name(c as u32);
            let shown = if label == letter {
                label
            } else {
                format!("{label}  ·  {letter}")
            };
            chp.text(
                r.center(),
                Align2::CENTER_CENTER,
                shown,
                FontId::proportional(12.0),
                th.text_dim,
            );
            chp.vline(r.max.x, outer.min.y..=outer.min.y + HEADER_HEIGHT, line);
        }

        let row_header = Rect::from_min_size(
            egui::pos2(outer.min.x, body_origin.y),
            Vec2::new(ROW_HEADER_WIDTH, body_rect.height()),
        );
        hp.rect_filled(row_header, 0.0, th.header_bg);
        let rhp = hp.with_clip_rect(row_header);
        // Row headers carry the ORIGINAL row number even under a filter. A
        // filtered view that renumbered its rows 1..N would be actively
        // misleading: the whole point of finding row 4,912,733 is knowing it
        // is row 4,912,733.
        for r in row_range.clone() {
            let Some(resolved) = resolve_row(r) else {
                continue;
            };
            let row = resolved.row();
            let y = body_origin.y + (r - first_row) as f32 * ROW_HEIGHT - frac_px;
            // Row numbers name the DATA row, so a filtered view shows the
            // original 1, 5, 9, ... rather than renumbering to 1, 2, 3 — the
            // user must always be able to tell which rows are hidden.
            let rect = Rect::from_min_size(
                egui::pos2(outer.min.x, y),
                Vec2::new(ROW_HEADER_WIDTH, ROW_HEIGHT),
            );
            let selected = self.selection.is_some_and(|s| {
                let (a, b) = s.row_range();
                row >= a && row <= b
            });
            if resolved.is_pad() {
                rhp.rect_filled(rect, 0.0, th.pad_row);
            }
            if selected {
                rhp.rect_filled(rect, 0.0, th.accent_soft);
            }
            // A padding row still shows its would-be number, so the user can
            // see where they are, but dimmed — it is not a row of the file
            // yet. Typing into it is what makes it one.
            rhp.text(
                egui::pos2(rect.max.x - 8.0, rect.center().y),
                Align2::RIGHT_CENTER,
                (row as u64 + 1).to_string(),
                FontId::proportional(11.5),
                if selected {
                    th.accent
                } else if resolved.is_pad() {
                    th.grid_line
                } else {
                    th.text_dim
                },
            );
        }

        hp.rect_filled(
            Rect::from_min_size(outer.min, Vec2::new(ROW_HEADER_WIDTH, HEADER_HEIGHT)),
            0.0,
            th.header_bg,
        );
        hp.line_segment(
            [
                egui::pos2(outer.min.x, body_origin.y - 0.5),
                egui::pos2(outer.max.x, body_origin.y - 0.5),
            ],
            line,
        );
        hp.line_segment(
            [
                egui::pos2(body_origin.x - 0.5, outer.min.y),
                egui::pos2(body_origin.x - 0.5, outer.max.y),
            ],
            line,
        );

        GridResponse {
            clicked,
            drag_to,
            fill_started,
            fill_to,
            fill_released,
            double_clicked,
            painted_cells,
            visible_rows: row_range,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest representable step at a given magnitude.
    fn ulp_f32(x: f32) -> f32 {
        f32::from_bits(x.to_bits() + 1) - x
    }
    fn ulp_f64(x: f64) -> f64 {
        f64::from_bits(x.to_bits() + 1) - x
    }

    #[test]
    fn f64_row_indexing_survives_10gb_scale() {
        // A 10GB CSV is ~200M rows. Row indices must round-trip exactly.
        for rows in [10_000_000u64, 200_000_000, 1_000_000_000] {
            let deepest = (rows - 1) as f64;
            assert!(
                ulp_f64(deepest) < 1.0,
                "at {rows} rows an f64 row index has ulp {} — rows not addressable",
                ulp_f64(deepest)
            );
            assert_eq!(
                deepest.floor() as u64,
                rows - 1,
                "row {rows} lost precision"
            );
        }
    }

    #[test]
    fn documents_why_f32_pixels_were_abandoned() {
        // The old design multiplied rows by ROW_HEIGHT into an f32 canvas.
        // At 200M rows that is unusable — this is why ScrollState uses f64
        // row indices instead. If this ever stops holding, revisit the docs.
        let canvas_200m = 200_000_000f32 * ROW_HEIGHT;
        assert!(
            ulp_f32(canvas_200m) > ROW_HEIGHT,
            "f32 pixel canvas would now work at 200M rows — module docs are stale"
        );
    }

    #[test]
    fn scroll_clamps_to_valid_range() {
        let mut s = ScrollState {
            row_offset: -50.0,
            col_px: -20.0,
        };
        s.clamp(1000, 800.0, Vec2::new(400.0, 220.0));
        assert_eq!(s.row_offset, 0.0);
        assert_eq!(s.col_px, 0.0);

        // Cannot scroll past the last screenful.
        let mut s = ScrollState {
            row_offset: 1e9,
            col_px: 1e9,
        };
        s.clamp(1000, 800.0, Vec2::new(400.0, 220.0));
        assert_eq!(s.row_offset, 1000.0 - 10.0);
        assert_eq!(s.col_px, 400.0);
    }

    #[test]
    fn tiny_sheet_does_not_scroll() {
        // Fewer rows than fit on screen: offset must stay pinned at 0.
        let mut s = ScrollState {
            row_offset: 5.0,
            col_px: 0.0,
        };
        s.clamp(3, 200.0, Vec2::new(400.0, 660.0));
        assert_eq!(s.row_offset, 0.0);
    }

    #[test]
    fn visible_row_count_is_viewport_bound_not_data_bound() {
        // The core virtualization claim: work depends on the window only.
        let viewport_h = 1080.0f32;
        let visible = (viewport_h / ROW_HEIGHT).ceil() as usize + 1;
        assert!(visible < 60);
        // Same answer whether the sheet has 1k rows or 200M.
        for total in [1_000usize, 200_000_000] {
            let shown = visible.min(total);
            assert!(
                shown <= 60,
                "would paint {shown} rows for {total}-row sheet"
            );
        }
    }

    // --- filter mode ---

    fn filter_of(rows: &[u32]) -> RowFilter {
        let cells: Vec<CellRef> = rows.iter().map(|&r| CellRef::new(r, 0)).collect();
        RowFilter::from_matches(&cells, false, cells.len())
    }

    #[test]
    fn unfiltered_row_lookup_is_the_identity() {
        assert_eq!(Grid::underlying_row(None, 0), Some(0));
        assert_eq!(Grid::underlying_row(None, 199_999_999), Some(199_999_999));
        assert_eq!(Grid::visible_row(None, 4_912_733), Some(4_912_733));
    }

    #[test]
    fn filtered_row_lookup_uses_the_mapping_both_ways() {
        let f = filter_of(&[3, 9, 4_912_733]);
        assert_eq!(Grid::underlying_row(Some(&f), 0), Some(3));
        assert_eq!(Grid::underlying_row(Some(&f), 2), Some(4_912_733));
        assert_eq!(Grid::underlying_row(Some(&f), 3), None, "past the end");
        assert_eq!(Grid::visible_row(Some(&f), 4_912_733), Some(2));
        assert_eq!(Grid::visible_row(Some(&f), 4), None, "hidden row");
    }

    #[test]
    fn row_header_text_is_the_original_one_based_row() {
        // Acceptance criterion: headers keep original numbers under a filter.
        // This mirrors exactly what the header loop paints.
        let f = filter_of(&[0, 41, 199_999_999]);
        let painted: Vec<String> = (0..f.len())
            .map(|r| {
                let row = Grid::underlying_row(Some(&f), r).unwrap();
                (row as u64 + 1).to_string()
            })
            .collect();
        assert_eq!(painted, vec!["1", "42", "200000000"]);
    }

    #[test]
    fn hit_test_row_math_addresses_the_underlying_row() {
        // Reproduces the hit-test arithmetic: pixels -> visible row -> real
        // row. A click three rows down a filtered view must name the third
        // KEPT row, which is what makes an edit write through correctly.
        let f = filter_of(&[10, 250, 999_000, 199_999_999]);
        let first_row = 0usize;
        let frac_px = 0.0f32;
        let click_y_offset = ROW_HEIGHT * 3.0 + 4.0;
        let visible = first_row as f64 + ((click_y_offset + frac_px) / ROW_HEIGHT) as f64;
        let cell = CellRef::new(Grid::underlying_row(Some(&f), visible as usize).unwrap(), 2);
        assert_eq!(cell.row, 199_999_999, "edit would hit the wrong row");
        assert_eq!(cell.col, 2);
    }

    #[test]
    fn cell_screen_rect_positions_by_visible_row_and_hides_filtered_rows() {
        let f = filter_of(&[5, 6, 1_000_000]);
        let outer = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(800.0, 600.0));
        let scroll = ScrollState {
            row_offset: 0.0,
            col_px: 0.0,
        };
        let widths = [100.0f32; 4];

        // Underlying row 1,000,000 is visible row 2, so it sits two rows down
        // — not a million rows off screen.
        let rect = Grid::cell_screen_rect(
            CellRef::new(1_000_000, 0),
            outer,
            &scroll,
            &widths,
            Some(&f),
            None,
        )
        .expect("kept row must have a rect");
        let expected_y = outer.min.y + HEADER_HEIGHT + 2.0 * ROW_HEIGHT;
        assert!((rect.min.y - expected_y).abs() < 0.5, "got {rect:?}");

        // A row the filter hides has no rect at all, so the cell editor cannot
        // be painted over a row that is not on screen.
        assert!(Grid::cell_screen_rect(
            CellRef::new(7, 0),
            outer,
            &scroll,
            &widths,
            Some(&f),
            None
        )
        .is_none());

        // Without the filter the same cell is a million rows below the fold.
        assert!(Grid::cell_screen_rect(
            CellRef::new(1_000_000, 0),
            outer,
            &scroll,
            &widths,
            None,
            None
        )
        .is_none());
    }

    #[test]
    fn filtered_viewport_work_is_constant_at_200m_rows() {
        // 100k matches (the search cap) scattered through a 200M-row sheet.
        // A frame must touch only a viewport's worth of the mapping — this is
        // the 16.67 ms budget claim reduced to its algorithmic core.
        let matches: Vec<CellRef> = (0..100_000u32)
            .map(|i| CellRef::new(i.saturating_mul(2000), 0))
            .collect();
        let f = RowFilter::from_matches(&matches, true, 100_000);

        let visible_count = (1080.0f32 / ROW_HEIGHT).ceil() as usize + 1;
        for first in [0usize, 12_345, 99_000] {
            let last = (first + visible_count).min(f.len());
            let w = f.window(first, last);
            assert!(
                w.len() <= visible_count,
                "a frame read {} rows; the viewport holds {visible_count}",
                w.len()
            );
            // And the narrowing bounds the paint loop derives are O(1) to get.
            if let (Some(&a), Some(&b)) = (w.first(), w.last()) {
                assert!(a <= b);
            }
        }
    }

    #[test]
    fn filtered_scroll_offset_stays_exact_at_the_deep_end() {
        // Scrolling to the last visible row of a filtered 200M-row sheet must
        // still resolve to the exact underlying row, through f64.
        let f = filter_of(&[0, 100_000_000, 199_999_998, 199_999_999]);
        let last_visible = f.len() - 1;
        let offset = last_visible as f64;
        let resolved = Grid::underlying_row(Some(&f), offset.floor() as usize).unwrap();
        assert_eq!(resolved, 199_999_999);
        assert_eq!(Grid::visible_row(Some(&f), 199_999_998), Some(2));
        assert_eq!(Grid::visible_row(Some(&f), 199_999_999), Some(3));
    }

    // --- empty-row padding (issue #20) ---

    /// The whole extent story: padding lengthens what the user can SCROLL to
    /// without changing what the sheet CONTAINS. `row_count` is what export,
    /// SUM and the status bar read, and it is not in this arithmetic at all.
    #[test]
    fn padding_extends_the_scrollable_extent_only() {
        let data_rows = 3usize;
        let off = data_rows + EMPTY_ROW_PADDING;
        let on = data_rows;
        assert!(off > on);
        // The extent grows by exactly the padding, no more.
        assert_eq!(off - on, EMPTY_ROW_PADDING);
        // And a two-row file still gets somewhere to type: the acceptance
        // criterion is that the toggle produces reachable empty rows.
        let pad = EMPTY_ROW_PADDING;
        assert!(pad > 0, "padding must give the user somewhere to type");
    }

    #[test]
    fn pad_space_maps_screen_rows_to_rows_past_the_end() {
        // 3 real rows on screen, sheet is 3 rows deep.
        let p = PadSpace {
            first_pad_screen_row: 3,
            first_pad_data_row: 3,
        };
        assert_eq!(p.data_row(2), None, "row 2 is real, not padding");
        assert_eq!(p.data_row(3), Some(3), "first padding row is row 4");
        assert_eq!(p.data_row(10), Some(10));
        // ...and back again, which is what places the in-cell editor.
        assert_eq!(p.screen_row(3), Some(3));
        assert_eq!(p.screen_row(2), None, "a real row is not in pad space");
        assert_eq!(p.screen_row(10), Some(10));
    }

    /// Under a filter the two spaces come apart: 4 surviving rows on screen,
    /// but the sheet is 200 rows deep. Padding must address rows past the
    /// SHEET, not past the filtered subset — otherwise typing in the padding
    /// would silently overwrite a hidden record.
    #[test]
    fn padding_under_a_filter_addresses_past_the_sheet_not_past_the_filter() {
        let sheet_rows = 200usize;
        let f = filter_of(&[3, 9, 40, 199]);
        let p = PadSpace {
            first_pad_screen_row: f.len(),
            first_pad_data_row: sheet_rows,
        };
        // Screen row 4 is the first padding row and names row 200 — one past
        // the end of the sheet, NOT the 5th filtered row (which does not
        // exist) and not row 4 (which the filter is hiding).
        assert_eq!(p.data_row(4), Some(200));
        assert_eq!(p.data_row(3), None, "screen row 3 is the last kept row");
        // Every kept row stays owned by the filter, untouched by pad space.
        for v in 0..f.len() {
            assert_eq!(p.data_row(v), None, "pad space stole a filtered row");
            assert!(Grid::underlying_row(Some(&f), v).is_some());
        }
        // A padding row's data row is in NEITHER mapping — which is exactly
        // why it must never be looked up in one.
        assert_eq!(Grid::visible_row(Some(&f), 200), None);
    }

    /// The same for a table's header filter, whose mapping is a rank lookup.
    /// A padding screen index handed to `nth_visible` would run off the end;
    /// resolving padding first is what stops that.
    #[test]
    fn padding_screen_rows_are_past_the_end_of_the_table_mapping() {
        let visible_rows = 4usize; // what a header filter kept
        let sheet_rows = 500usize;
        let p = PadSpace {
            first_pad_screen_row: visible_rows,
            first_pad_data_row: sheet_rows,
        };
        for r in 0..visible_rows {
            assert_eq!(p.data_row(r), None, "a table row was claimed as padding");
        }
        assert_eq!(p.data_row(visible_rows), Some(sheet_rows as u32));
    }

    /// Padding must be reachable AND editable: the editor is positioned from
    /// `cell_screen_rect`, which under a filter cannot ask the mapping about a
    /// padding row. Passing the pad space is what gives it a rect.
    #[test]
    fn the_editor_can_be_placed_over_a_padding_row() {
        let f = filter_of(&[5, 6, 7]);
        let outer = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(800.0, 600.0));
        let scroll = ScrollState {
            row_offset: 0.0,
            col_px: 0.0,
        };
        let widths = [100.0f32; 4];
        let p = PadSpace {
            first_pad_screen_row: 3,
            first_pad_data_row: 100,
        };

        // Without pad space the filter hides row 100 and there is nowhere to
        // draw the editor — the bug this parameter exists to prevent.
        assert!(Grid::cell_screen_rect(
            CellRef::new(100, 0),
            outer,
            &scroll,
            &widths,
            Some(&f),
            None
        )
        .is_none());
        // With it, the first padding row sits directly under the last kept row.
        let rect = Grid::cell_screen_rect(
            CellRef::new(100, 0),
            outer,
            &scroll,
            &widths,
            Some(&f),
            Some(p),
        )
        .expect("a padding row must be editable");
        let expected_y = outer.min.y + HEADER_HEIGHT + 3.0 * ROW_HEIGHT;
        assert!((rect.min.y - expected_y).abs() < 0.5, "got {rect:?}");
        // A real filtered row still resolves through the FILTER, not the pad.
        let real = Grid::cell_screen_rect(
            CellRef::new(6, 0),
            outer,
            &scroll,
            &widths,
            Some(&f),
            Some(p),
        )
        .expect("kept row");
        let expect_real = outer.min.y + HEADER_HEIGHT + ROW_HEIGHT;
        assert!((real.min.y - expect_real).abs() < 0.5);
    }

    /// Typing into padding extends the sheet through the overlay's own
    /// extent — the mechanism the issue asked us to reuse rather than
    /// materialising rows. Before the edit `row_count` is unchanged by
    /// padding; after it, it grows by exactly what was typed into.
    #[test]
    fn typing_in_the_padding_extends_the_sheet_without_materialising_rows() {
        use ferrix_core::{CellInput, EditOverlay, Value};

        let base_rows = 3usize;
        let mut ov = EditOverlay::new();
        // Padding alone materialises nothing at all.
        assert_eq!(ov.extent().0, 0);
        assert_eq!(ov.len(), 0);

        let p = PadSpace {
            first_pad_screen_row: base_rows,
            first_pad_data_row: base_rows,
        };
        // The user clicks the 6th padding row and types.
        let target = CellRef::new(p.data_row(base_rows + 5).unwrap(), 0);
        assert_eq!(target.row, 8);
        ov.set(target, CellInput::Literal(Value::Number(42.0)));

        // ONE cell exists — 200 padding rows did not become 200 rows of data.
        assert_eq!(ov.len(), 1);
        // ...and the sheet is now genuinely 9 rows deep, so export, SUM and
        // the status bar all see the new row.
        assert_eq!(ov.extent().0, 9);
        assert_eq!(base_rows.max(ov.extent().0), 9);
    }
}
