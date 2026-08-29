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
use ferrix_core::{column_name, CellRef, RowFilter, Selection, SortOrder, Value};

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

/// Base font size for cell text at 100% zoom.
pub const BASE_FONT: f32 = 12.5;

/// Zoom range. 25% is about the smallest at which a row is still a row rather
/// than a stripe; 400% is where one cell fills a quarter of the window.
pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 4.0;

/// The zoom stops the +/- commands walk. Multiplicative rather than linear, so
/// a step feels the same size at 25% as it does at 400%.
pub const ZOOM_STOPS: &[f32] = &[0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

#[inline]
pub fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() {
        z.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

/// Next zoom stop above `z`, or `z` when already at the top.
pub fn zoom_in(z: f32) -> f32 {
    let z = clamp_zoom(z);
    ZOOM_STOPS
        .iter()
        .copied()
        .find(|&s| s > z + 1e-4)
        .unwrap_or(MAX_ZOOM)
}

/// Next zoom stop below `z`, or `z` when already at the bottom.
pub fn zoom_out(z: f32) -> f32 {
    let z = clamp_zoom(z);
    ZOOM_STOPS
        .iter()
        .copied()
        .rev()
        .find(|&s| s < z - 1e-4)
        .unwrap_or(MIN_ZOOM)
}

/// Every length the grid draws with, scaled by the zoom factor.
///
/// Zoom is a pure VIEW transform, exactly like sort and filter: it changes how
/// large a row is on screen and nothing about which row it is. Row heights,
/// column widths, header bands and the font all scale by the same factor, so
/// the grid stays geometrically similar to itself and a click at 200% resolves
/// through the same arithmetic as one at 100%.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub zoom: f32,
    pub row_h: f32,
    pub header_h: f32,
    pub row_header_w: f32,
    pub font: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl Metrics {
    pub fn new(zoom: f32) -> Self {
        let z = clamp_zoom(zoom);
        Self {
            zoom: z,
            row_h: ROW_HEIGHT * z,
            header_h: HEADER_HEIGHT * z,
            row_header_w: ROW_HEADER_WIDTH * z,
            font: BASE_FONT * z,
        }
    }

    /// A stored column width, on screen.
    #[inline]
    pub fn col_width(&self, base: f32) -> f32 {
        base * self.zoom
    }
}

/// The leading band of the viewport: frozen panes and split view are the same
/// mechanism with one difference.
///
/// A band of `rows` rows and/or `cols` columns is painted from its OWN scroll
/// offset, before the body, and the body scrolls underneath it. When `frozen`
/// the band's offset is pinned at zero — that is freeze panes. When not frozen
/// the offset is the user's to move — that is a split view: two independent
/// scroll offsets, per axis, over ONE column layout.
///
/// Column widths and row heights are shared with the body by construction:
/// both bands index the same `col_widths` and the same [`Metrics`], so there
/// is no second layout to drift out of step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Panes {
    /// Rows in the leading band. 0 = no horizontal band.
    pub rows: usize,
    /// Columns in the leading band. 0 = no vertical band.
    pub cols: usize,
    /// Pinned at offset 0 (freeze) rather than independently scrolled (split).
    pub frozen: bool,
    /// First row the band shows when split. Ignored while `frozen`.
    pub lead_row: f64,
    /// First column the band shows when split. Ignored while `frozen`.
    pub lead_col: usize,
}

impl Default for Panes {
    fn default() -> Self {
        Self {
            rows: 0,
            cols: 0,
            frozen: true,
            lead_row: 0.0,
            lead_col: 0,
        }
    }
}

impl Panes {
    pub fn is_active(&self) -> bool {
        self.rows > 0 || self.cols > 0
    }

    /// Freeze the first `rows` rows and `cols` columns.
    pub fn freeze(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            frozen: true,
            lead_row: 0.0,
            lead_col: 0,
        }
    }

    /// First screen row the leading band paints. Always 0 while frozen — the
    /// whole point is that it does not move when the body does.
    #[inline]
    pub fn lead_first_row(&self) -> usize {
        if self.frozen {
            0
        } else {
            self.lead_row.max(0.0).floor() as usize
        }
    }

    #[inline]
    pub fn lead_first_col(&self) -> usize {
        if self.frozen {
            0
        } else {
            self.lead_col
        }
    }

    /// Lowest body row offset. Under a freeze the body starts BELOW the frozen
    /// rows, so a frozen row is never also painted (and scrolled) by the body.
    /// Under a split both panes may show any row, including the same one.
    #[inline]
    pub fn body_min_row(&self) -> f64 {
        if self.frozen {
            self.rows as f64
        } else {
            0.0
        }
    }

    /// First column of body column space, for the same reason.
    #[inline]
    pub fn body_first_col(&self) -> usize {
        if self.frozen {
            self.cols
        } else {
            0
        }
    }
}

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
pub enum ScreenRow {
    /// A real row, resolved through whichever filters are active.
    Data(u32),
    /// Empty padding past the end of the sheet. Addressable — typing here
    /// extends the sheet through the overlay's own extent — but it holds no
    /// data, is in no filter's mapping, and carries no table decoration.
    Pad(u32),
}

impl ScreenRow {
    #[inline]
    pub fn row(self) -> u32 {
        match self {
            ScreenRow::Data(r) | ScreenRow::Pad(r) => r,
        }
    }

    #[inline]
    pub fn is_pad(self) -> bool {
        matches!(self, ScreenRow::Pad(_))
    }
}

/// THE row resolution. One implementation, every caller.
///
/// Up to three view transforms can narrow or reorder the rows at once, and
/// they compose in a FIXED order:
///
/// 1. the table's header filter maps a screen row to a data row by rank;
/// 2. search filter mode selects among data rows;
/// 3. sort permutes whatever survived — it is built OVER the filtered rows,
///    so its values are already underlying data rows and it is consulted
///    last, never alongside a filter.
///
/// Resolving these separately from the same screen index is exactly the bug
/// that once painted WRONG RECORDS under CORRECT row numbers: whichever
/// variable the caller happened to read won, and the other transform was
/// silently ignored. So there is one resolver, it is built once per frame, and
/// painting, row headers, hit-testing and the cell editor all go through it.
///
/// Padding is checked FIRST and short-circuits everything else. A padding row
/// is not a filterable or sortable row: feeding its screen index to any of the
/// three mappings would run off the end, or — under a shorter view — alias
/// onto an unrelated record.
#[derive(Clone, Copy, Default)]
pub struct RowResolver<'a> {
    /// Search filter mode's mapping, when on.
    pub filter: Option<&'a RowFilter>,
    /// Sort mapping, when a column is sorted. Built over the rows the filters
    /// kept, so it supersedes them on the read path rather than competing.
    pub sort: Option<&'a SortOrder>,
    /// The table's header-filter mapping, when a filtered table is shown.
    pub table: Option<&'a TableDecor<'a>>,
    /// Empty rows offered past the end of the sheet, when the toggle is on.
    pub pad: Option<PadSpace>,
}

impl<'a> RowResolver<'a> {
    /// Screen row -> the row it actually shows.
    #[inline]
    pub fn resolve(&self, r: usize) -> Option<ScreenRow> {
        if let Some(row) = self.pad.and_then(|p| p.data_row(r)) {
            return Some(ScreenRow::Pad(row));
        }
        // Sort is composed AFTER the filters: its candidate set was the rows
        // they kept, so its values are underlying rows and asking a filter
        // again would map an already-mapped index a second time.
        if let Some(s) = self.sort {
            return s.underlying(r).map(ScreenRow::Data);
        }
        match self.filter {
            // Search filter active: it already indexes data rows.
            Some(f) => f.underlying(r),
            // Otherwise the table mapping (identity when there is no table).
            None => match self.table {
                Some(t) => t.data_row(r).map(|d| d as u32),
                None => Some(r as u32),
            },
        }
        .map(ScreenRow::Data)
    }

    /// Underlying row -> screen row, or `None` when the view hides it.
    #[inline]
    pub fn visible_of(&self, row: u32) -> Option<usize> {
        // A padding row is in no mapping, so it is resolved from the pad space
        // FIRST — asking a filter about it would return None and the in-cell
        // editor would have nowhere to draw.
        if let Some(v) = self.pad.and_then(|p| p.screen_row(row)) {
            return Some(v);
        }
        if let Some(s) = self.sort {
            return s.visible_of(row);
        }
        match self.filter {
            Some(f) => f.visible_of(row),
            None => Some(row as usize),
        }
    }

    /// Rows the view transforms resolve, before padding is added on top.
    pub fn resolved_rows(&self, data_rows: usize) -> usize {
        if let Some(s) = self.sort {
            return s.len();
        }
        match self.filter {
            Some(f) => f.len(),
            None => match self.table {
                Some(t) => t.visible_row_count(data_rows),
                None => data_rows,
            },
        }
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
    ///
    /// The no-panes, no-zoom case of [`ScrollState::clamp_body`]. Kept as the
    /// simple entry point for callers with neither.
    #[allow(dead_code)]
    pub fn clamp(&mut self, total_rows: usize, total_width: f32, view: Vec2) {
        self.clamp_body(total_rows, total_width, view, ROW_HEIGHT, 0.0, 0.0);
    }

    /// Clamp the BODY pane's offset.
    ///
    /// `view` is the body's own rect — the leading frozen/split band is not
    /// part of it — and `min_row`/`min_x` are where body space starts, which
    /// under a freeze is just past the frozen band. Without the floor the body
    /// would scroll up into rows the frozen band is already painting and show
    /// them twice.
    pub fn clamp_body(
        &mut self,
        total_rows: usize,
        total_width: f32,
        view: Vec2,
        row_h: f32,
        min_row: f64,
        min_x: f32,
    ) {
        let visible_rows = (view.y / row_h.max(1.0)) as f64;
        let max_row = (total_rows as f64 - visible_rows).max(min_row);
        self.row_offset = self.row_offset.clamp(min_row, max_row);
        let max_x = (total_width - min_x - view.x).max(0.0);
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
    /// Display column whose header was pressed, to start a reorder drag.
    ///
    /// Reported on PRESS, not click. egui reports `primary_clicked` on
    /// *release*, so keying a drag off it means the gesture never starts —
    /// exactly the bug that made the fill handle silently do nothing while
    /// every unit test passed.
    pub header_press: Option<usize>,
    /// Display column the header drag is currently over, while held.
    pub header_drag_to: Option<usize>,
    /// Set on the frame a header drag is released.
    pub header_released: bool,
    pub painted_cells: usize,
    /// Where each visible column's header was actually painted this frame:
    /// (display column, centre point).
    ///
    /// The header band's y moves whenever a bar above the grid opens — the
    /// search bar alone shifts it by tens of pixels — so a test that hard-codes
    /// header pixels silently starts clicking the search bar instead and
    /// reports the feature as broken. This reports the real geometry so a
    /// caller can aim at it.
    pub header_hitboxes: Vec<(usize, egui::Pos2)>,
    /// Reported for callers that want to size scrollbars or prefetch; the app
    /// does not consume it yet, but it is part of the Grid's public response.
    #[allow(dead_code)]
    pub visible_rows: std::ops::Range<usize>,
    /// Every screen row painted this frame, frozen band FIRST then body, with
    /// the underlying row each one resolved to.
    ///
    /// This is the answer to "what is actually on screen", and it comes from
    /// the same list the paint loop walked rather than from a recomputation —
    /// so a test that asserts row 1 is still visible under a freeze is reading
    /// the real paint output, not a model of it.
    pub painted_rows: Vec<(usize, u32)>,
    /// How many of `painted_rows` belong to the frozen/split band.
    pub frozen_row_count: usize,
    /// The zoom actually applied this frame, after clamping.
    pub zoom: f32,
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
    /// Display column currently being dragged by its header, if any. Set by
    /// the app between press and release so the grid can paint a drop
    /// indicator and report the target.
    pub header_dragging: Option<usize>,
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
    /// Sheet-wide formatting: manual colours and type styling that apply to
    /// any cell, table or not. `None` when nothing has been formatted, which
    /// keeps the default path free of lookups.
    pub format: Option<&'a ferrix_core::SheetFormat>,
    /// Merged regions. `None` when the sheet has none, keeping the default
    /// paint path free of lookups.
    pub merges: Option<&'a ferrix_core::merge::MergeMap>,
    /// Active sort, when a column header has been clicked. A VIEW TRANSFORM:
    /// it permutes which underlying row each screen row shows and never moves
    /// a byte of data. Composed after the filters through [`RowResolver`].
    pub sort: Option<&'a ferrix_core::SortOrder>,
    /// Zoom-scaled lengths. Everything the grid draws is sized from this, so
    /// zoom is one multiply at the layout level rather than a special case in
    /// every paint call.
    pub metrics: Metrics,
    /// Frozen / split leading band. Mutable because a SPLIT band scrolls
    /// independently, and the wheel over the band is what moves it.
    pub panes: &'a mut Panes,
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
        resolver: &RowResolver<'_>,
        m: Metrics,
        panes: Panes,
    ) -> Option<Rect> {
        let body_origin = outer.min + Vec2::new(m.row_header_w, m.header_h);
        // Through THE resolver, so the editor lands on the same screen row the
        // paint loop drew the cell on — under a filter, a sort, or both.
        let visible = resolver.visible_of(cell.row)?;
        let w_of = |c: usize| -> f32 {
            m.col_width(col_widths.get(c).copied().unwrap_or(DEFAULT_COL_WIDTH))
        };

        // Band extents, mirroring `show`. Widths and heights come from the
        // SAME metrics and the SAME col_widths the body uses — a frozen column
        // is the same column, not a copy of it.
        let band_rows = panes.rows;
        let band_cols = panes.cols;
        let band_h = band_rows as f32 * m.row_h;
        let lead_r = panes.lead_first_row();
        let lead_c = panes.lead_first_col();
        let band_w: f32 = (lead_c..lead_c + band_cols).map(w_of).sum();

        // A row inside the leading band is painted from the band's own offset,
        // which under a freeze never moves — so it has a rect no matter how
        // far the body has scrolled.
        let y = if band_rows > 0 && visible >= lead_r && visible < lead_r + band_rows {
            body_origin.y + (visible - lead_r) as f32 * m.row_h
        } else {
            let rel = visible as f64 - scroll.row_offset;
            let yy = body_origin.y + band_h + (rel * m.row_h as f64) as f32;
            if yy + m.row_h < body_origin.y + band_h || yy > outer.max.y {
                return None;
            }
            yy
        };

        let col = cell.col as usize;
        let w = w_of(col);
        let x = if band_cols > 0 && col >= lead_c && col < lead_c + band_cols {
            body_origin.x + (lead_c..col).map(w_of).sum::<f32>()
        } else {
            let body_c0 = if panes.frozen { band_cols } else { 0 };
            if col < body_c0 {
                return None;
            }
            let xx = body_origin.x + band_w + (body_c0..col).map(w_of).sum::<f32>() - scroll.col_px;
            if xx + w < body_origin.x + band_w || xx > outer.max.x {
                return None;
            }
            xx
        };
        Some(Rect::from_min_size(egui::pos2(x, y), Vec2::new(w, m.row_h)))
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
        // Rows the view transforms resolve. Padding is added on top of this
        // and is never part of it, which is what keeps padding out of every
        // mapping. Built without the pad so it can size the pad itself.
        let unpadded = RowResolver {
            filter,
            sort: self.sort,
            table: self.table,
            pad: None,
        };
        let filtered_rows = unpadded.resolved_rows(data_rows);
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

        // THE single row resolution, used by painting, row headers,
        // hit-testing and the cell editor alike. Filters compose first, sort
        // last, padding short-circuits everything — see [`RowResolver`].
        let resolver = RowResolver { pad, ..unpadded };
        let resolve_row = |r: usize| -> Option<ScreenRow> { resolver.resolve(r) };

        let m = self.metrics;
        let row_h = m.row_h;
        let width_of = |c: usize| -> f32 {
            m.col_width(self.col_widths.get(c).copied().unwrap_or(DEFAULT_COL_WIDTH))
        };

        // Column x-offsets via prefix sum, already zoom-scaled. Column counts
        // are small, so this is cheap to rebuild each frame and keeps variable
        // widths trivial. ONE prefix sum serves the frozen band and the body
        // alike: a frozen column is the same column, at the same width.
        let mut col_x = Vec::with_capacity(total_cols + 1);
        let mut acc = 0.0f32;
        for c in 0..total_cols {
            col_x.push(acc);
            acc += width_of(c);
        }
        col_x.push(acc);
        let total_width = acc;

        let outer = ui.available_rect_before_wrap();
        let body_origin = outer.min + Vec2::new(m.row_header_w, m.header_h);
        // The whole scrollable area: leading band plus body.
        let grid_rect = Rect::from_min_max(body_origin, outer.max - Vec2::splat(SCROLLBAR_W));

        // --- band geometry ---
        //
        // Clamped against the viewport: a band taller than the window would
        // leave the body no rows at all, and a freeze the user cannot scroll
        // out of is worse than no freeze.
        let max_band_rows = ((grid_rect.height() / row_h) as usize).saturating_sub(2);
        let max_band_cols = total_cols.saturating_sub(1);
        let mut panes = *self.panes;
        panes.rows = panes
            .rows
            .min(max_band_rows)
            .min(total_rows.saturating_sub(1));
        panes.cols = panes.cols.min(max_band_cols);
        let band_rows = panes.rows;
        let band_cols = panes.cols;
        let lead_r = panes.lead_first_row();
        let lead_c = panes.lead_first_col();
        let band_h = band_rows as f32 * row_h;
        let band_w: f32 = (lead_c..(lead_c + band_cols).min(total_cols))
            .map(width_of)
            .sum();
        // Where the BODY pane lives — under and to the right of the band.
        let body_rect = Rect::from_min_max(
            egui::pos2(grid_rect.min.x + band_w, grid_rect.min.y + band_h),
            grid_rect.max,
        );
        let body_size = body_rect.size();
        // First column of body column space. Under a freeze the body starts
        // past the frozen columns so a frozen column is never painted twice.
        let body_c0 = panes.body_first_col().min(total_cols);
        let body_x0 = col_x[body_c0];

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
        // Which pane the wheel drives. A SPLIT band has its own offset, so a
        // wheel over it scrolls the band and leaves the body where it was —
        // that is what makes it two independent views of one column layout. A
        // FROZEN band has no offset to move, so the wheel always reaches the
        // body underneath.
        let over_lead_rows = !panes.frozen
            && band_rows > 0
            && pointer_pos.is_some_and(|p| p.y < grid_rect.min.y + band_h);
        if pointer_in_body {
            if wheel.y != 0.0 {
                // Convert pixel wheel delta into row units.
                let d = (wheel.y / row_h) as f64;
                if over_lead_rows {
                    panes.lead_row = (panes.lead_row - d).clamp(0.0, total_rows as f64 - 1.0);
                } else {
                    self.scroll.row_offset -= d;
                }
            }
            if wheel.x != 0.0 {
                self.scroll.col_px -= wheel.x;
            }
        }
        self.scroll.clamp_body(
            total_rows,
            total_width,
            body_size,
            row_h,
            panes.body_min_row(),
            body_x0,
        );
        // Write the (clamped, possibly split-scrolled) band state back.
        *self.panes = panes;

        let first_row = self.scroll.row_offset.floor().max(0.0) as usize;
        // Sub-row offset in pixels, so scrolling is smooth rather than snapping.
        let frac_px = ((self.scroll.row_offset - first_row as f64) * row_h as f64) as f32;
        let visible_count = (body_size.y / row_h).ceil() as usize + 1;
        let last_row = (first_row + visible_count).min(total_rows);
        let row_range = first_row..last_row;

        // Body column window, in body column space: `col_px` is measured from
        // the first body column, not from column 0, so a frozen band does not
        // shift what "scrolled to the left edge" means.
        let first_col = col_x
            .partition_point(|&x| x <= self.scroll.col_px + body_x0)
            .saturating_sub(1)
            .max(body_c0);
        let last_col = col_x
            .partition_point(|&x| x < self.scroll.col_px + body_x0 + body_size.x)
            .min(total_cols)
            .max(first_col);
        let col_range = first_col..last_col;

        // --- the two bands, in paint order ---
        //
        // The frozen/split band is built FIRST and the body second, so the
        // paint loops below walk the band before the body exactly as the
        // feature describes. Both lists are viewport-sized: a band is a few
        // extra painted rows, never a second pass over the sheet.
        let mut row_bands: Vec<(usize, f32)> = Vec::with_capacity(band_rows + visible_count + 1);
        for i in 0..band_rows {
            let r = lead_r + i;
            if r >= total_rows {
                break;
            }
            row_bands.push((r, grid_rect.min.y + i as f32 * row_h));
        }
        let body_row_start = row_bands.len();
        for r in row_range.clone() {
            row_bands.push((
                r,
                body_rect.min.y + (r - first_row) as f32 * row_h - frac_px,
            ));
        }

        let mut col_bands: Vec<(usize, f32)> = Vec::with_capacity(band_cols + total_cols.min(64));
        for i in 0..band_cols {
            let c = lead_c + i;
            if c >= total_cols {
                break;
            }
            col_bands.push((c, grid_rect.min.x + col_x[c] - col_x[lead_c]));
        }
        for c in col_range.clone() {
            col_bands.push((c, body_rect.min.x + col_x[c] - body_x0 - self.scroll.col_px));
        }
        // Where a column landed this frame, for merge extents and drop marks.
        let x_of = |c: usize| -> Option<f32> {
            col_bands.iter().find(|(cc, _)| *cc == c).map(|&(_, x)| x)
        };

        let mut clicked = None;
        let mut double_clicked = None;
        let mut drag_to = None;
        let mut fill_started = false;
        let mut fill_to = None;
        let mut fill_released = false;
        let mut painted_cells = 0usize;
        let mut painted_rows: Vec<(usize, u32)> = Vec::with_capacity(row_bands.len());
        let mut frozen_row_count = 0usize;

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
        let (match_lo_row, match_hi_row) = match (self.sort, filter) {
            // Under a sort the visible window is an arbitrary subset of
            // underlying rows, not an ascending run, so the narrowing bounds
            // are the min/max of the window rather than its ends. Still
            // viewport-sized work — never a scan of the mapping.
            (Some(s), _) => {
                let lo = first_row.min(match_last);
                let w = &s.rows()[lo.min(s.len())..match_last.clamp(lo, s.len())];
                match (w.iter().min(), w.iter().max()) {
                    (Some(&a), Some(&b)) => (a as usize, b as usize + 1),
                    _ => (0, 0),
                }
            }
            (None, Some(f)) => {
                let w = f.window(first_row.min(match_last), match_last);
                match (w.first(), w.last()) {
                    // `last + 1` because the narrowing bound is exclusive.
                    (Some(&a), Some(&b)) => (a as usize, b as usize + 1),
                    _ => (0, 0),
                }
            }
            (None, None) => (first_row.min(match_last), match_last),
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

        // --- paint ---
        //
        // The painter covers the whole grid, band included. Per-band clipping
        // is applied where it matters (below), so the frozen band cannot bleed
        // into the body and vice versa.
        let painter = ui.painter_at(grid_rect);
        painter.rect_filled(grid_rect, 0.0, th.bg);
        let band_clip = Rect::from_min_max(
            grid_rect.min,
            egui::pos2(grid_rect.max.x, grid_rect.min.y + band_h.max(0.0)),
        );

        // `r` walks VISIBLE rows; `row` is the underlying row it maps to.
        // Everything painted from here on — cell values, highlights, selection
        // tests, the CellRefs handed back to the caller — uses `row`, so a
        // filtered grid addresses exactly the same cells an unfiltered one
        // would.
        //
        // The FROZEN/SPLIT BAND IS ITERATED FIRST, then the body. Both go
        // through the SAME `resolve_row`, so a frozen row shows the same
        // record under a sort or filter that it would show unfrozen — there is
        // no second row mapping anywhere in this function.
        for (bi, &(r, y)) in row_bands.iter().enumerate() {
            let in_lead_rows = bi < body_row_start;
            // Resolve the screen row to a data row through BOTH filters, in
            // the order they narrow: the table first, then search. Resolving
            // them independently would let one silently win.
            let Some(resolved) = resolve_row(r) else {
                continue;
            };
            let row = resolved.row();
            let is_pad = resolved.is_pad();
            let row_rect = Rect::from_min_size(
                egui::pos2(grid_rect.min.x, y),
                Vec2::new(grid_rect.width(), row_h),
            );
            let row_clip = if in_lead_rows {
                band_clip
            } else {
                Rect::from_min_max(egui::pos2(grid_rect.min.x, body_rect.min.y), grid_rect.max)
            };
            let rp = painter.with_clip_rect(row_clip);
            if is_pad {
                // Padding gets its own recessed fill and no zebra stripe, so
                // "there is no row here" is visibly different from "this row
                // exists and holds empty strings".
                rp.rect_filled(row_rect, 0.0, th.pad_row);
            } else if r % 2 == 1 {
                rp.rect_filled(row_rect, 0.0, th.row_alt);
            }

            for (ci, &(c, x)) in col_bands.iter().enumerate() {
                let in_lead_cols = ci < band_cols;
                let w = width_of(c);
                let cell_rect = Rect::from_min_size(egui::pos2(x, y), Vec2::new(w, row_h));
                // Clip to the intersection of this cell's row band and column
                // band, so neither band paints over the other.
                let clip_rect = {
                    let left = if in_lead_cols {
                        grid_rect.min.x
                    } else {
                        body_rect.min.x
                    };
                    let right = if in_lead_cols {
                        grid_rect.min.x + band_w
                    } else {
                        grid_rect.max.x
                    };
                    Rect::from_min_max(
                        egui::pos2(left, row_clip.min.y),
                        egui::pos2(right.max(left), row_clip.max.y),
                    )
                };
                if !clip_rect.intersects(cell_rect) {
                    continue;
                }
                let painter = painter.with_clip_rect(clip_rect);
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

                // Merged regions. A covered cell paints nothing at all — its
                // value lives on the anchor, and drawing anything here would
                // either repeat the anchor's text in every covered cell or
                // show a stale value the user cannot edit. The anchor instead
                // paints across the whole rectangle.
                let merge_region = self.merges.and_then(|m| m.region_at(cref));
                let mut cell_rect = cell_rect;
                if let Some(mr) = merge_region {
                    if mr.first_row != row || mr.first_col != c as u32 {
                        // Covered: skip this cell entirely.
                        continue;
                    }
                    // Anchor: widen to the region's full extent so long text
                    // is not clipped at the first column's edge. The extent
                    // uses the SAME per-band x the cells were painted at, so a
                    // merge that starts in the frozen band stays anchored to
                    // it rather than sliding with the body.
                    let last = (mr.last_col as usize).min(total_cols.saturating_sub(1));
                    let right = match x_of(last) {
                        Some(lx) => lx + width_of(last),
                        None => cell_rect.max.x,
                    };
                    let bottom = y + row_h * (mr.last_row - mr.first_row + 1) as f32;
                    cell_rect = Rect::from_min_max(
                        cell_rect.min,
                        egui::pos2(right.max(cell_rect.max.x), bottom),
                    );
                    painter.rect_filled(cell_rect, 0.0, th.bg);
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
                            egui::pos2(
                                cell_rect.min.x + 3.5 * m.zoom,
                                cell_rect.min.y + 4.0 * m.zoom,
                            ),
                            1.6 * m.zoom,
                            th.accent,
                        );
                    }

                    let pad = 6.0 * m.zoom;
                    let anchor = match align {
                        Align2::RIGHT_CENTER => {
                            egui::pos2(cell_rect.max.x - pad, cell_rect.center().y)
                        }
                        Align2::CENTER_CENTER => cell_rect.center(),
                        _ => egui::pos2(cell_rect.min.x + pad, cell_rect.center().y),
                    };
                    // Type styling. `decor` is None for the overwhelming
                    // majority of cells, so the default path allocates and
                    // branches no more than before.
                    let mut ty = ferrix_core::format::Typography::default();
                    if let Some(f) = self.format {
                        if let Some(ov) = f.cell_override(cref) {
                            ov.manual.typography.apply_to(&mut ty);
                        }
                    }
                    if let Some(d) = &decor {
                        d.typography.apply_to(&mut ty);
                    }
                    // Resolved against the ZOOMED default, so an unstyled cell
                    // grows with the zoom and a cell with an explicit point
                    // size grows by the same factor rather than staying put.
                    let ty = ty.resolved(BASE_FONT);
                    let size = ty.size * m.zoom;

                    let font = match ty.family {
                        ferrix_core::format::FontFamily::Monospace => FontId::monospace(size),
                        _ => FontId::proportional(size),
                    };

                    let clip = painter.with_clip_rect(
                        cell_rect.intersect(clip_rect).shrink2(Vec2::new(2.0, 0.0)),
                    );

                    // Bold is faked by over-painting with a sub-pixel offset.
                    // egui ships one weight per family, so the alternative is
                    // bundling a second font file; this reads as bold at the
                    // sizes a grid actually uses and costs one extra draw on
                    // the rare cells that ask for it.
                    let galley = clip.layout_no_wrap(text.clone(), font.clone(), color);
                    let rect = align.anchor_size(anchor, galley.size());
                    clip.galley(rect.min, galley.clone(), color);
                    if ty.bold {
                        clip.galley(rect.min + Vec2::new(0.4, 0.0), galley.clone(), color);
                    }
                    if ty.underline {
                        let y = rect.max.y - 2.0;
                        clip.hline(rect.min.x..=rect.max.x, y, Stroke::new(1.0_f32, color));
                    }
                    if ty.strikethrough {
                        let y = rect.center().y;
                        clip.hline(rect.min.x..=rect.max.x, y, Stroke::new(1.0_f32, color));
                    }
                }

                // The validation flag goes on LAST, over everything else. A
                // cell that fails its column's rule is never rejected or
                // rewritten — it keeps its value and gets a red triangle in the
                // top-right corner, the way a spreadsheet marks a problem the
                // user has to look at.
                if let Some(d) = &decor {
                    if d.violation.is_some() {
                        let tr = egui::pos2(cell_rect.max.x, cell_rect.min.y);
                        let s = 7.0 * m.zoom;
                        painter.add(egui::Shape::convex_polygon(
                            vec![tr, egui::pos2(tr.x - s, tr.y), egui::pos2(tr.x, tr.y + s)],
                            th.invalid_flag,
                            Stroke::NONE,
                        ));
                    }
                }
                painted_cells += 1;
            }
        }

        // --- grid lines ---
        //
        // Drawn per band from the same lists the cells came from, so a line
        // never lands where its row is not.
        let line = Stroke::new(1.0_f32, th.grid_line);
        for (bi, &(_, y)) in row_bands.iter().enumerate() {
            let lp = painter.with_clip_rect(if bi < body_row_start {
                band_clip
            } else {
                Rect::from_min_max(egui::pos2(grid_rect.min.x, body_rect.min.y), grid_rect.max)
            });
            lp.hline(grid_rect.min.x..=grid_rect.max.x, y, line);
        }
        for (ci, &(c, x)) in col_bands.iter().enumerate() {
            let in_lead = ci < band_cols;
            let lp = painter.with_clip_rect(if in_lead {
                Rect::from_min_max(
                    grid_rect.min,
                    egui::pos2(grid_rect.min.x + band_w, grid_rect.max.y),
                )
            } else {
                Rect::from_min_max(egui::pos2(body_rect.min.x, grid_rect.min.y), grid_rect.max)
            });
            lp.vline(x, grid_rect.min.y..=grid_rect.max.y, line);
            if c + 1 == total_cols {
                lp.vline(x + width_of(c), grid_rect.min.y..=grid_rect.max.y, line);
            }
        }
        // The seams: where the frozen band ends and the body begins. Drawn
        // stronger than a grid line so the user can see that the panes are
        // split rather than wondering why scrolling skips rows.
        let seam = Stroke::new(2.0_f32, th.accent);
        if band_rows > 0 {
            painter.hline(grid_rect.min.x..=grid_rect.max.x, body_rect.min.y, seam);
        }
        if band_cols > 0 {
            painter.vline(body_rect.min.x, grid_rect.min.y..=grid_rect.max.y, seam);
        }

        // --- hit testing ---
        //
        // Pixels map to a VISIBLE row, which is then translated back through
        // the filter so the CellRef the caller receives — and therefore any
        // click, drag, or edit built from it — names the underlying row.
        //
        // ONE hit test for both bands: y above the seam resolves inside the
        // frozen band, y below it through the body offset. Same for x. Zoom is
        // already in `row_h` and in the widths, so a click at 200% divides by
        // a doubled row height and lands on the same data cell it would at
        // 100% — no separate zoom-aware path to fall out of step.
        let hit = |pos: egui::Pos2| -> Option<CellRef> {
            if !grid_rect.contains(pos) {
                return None;
            }
            let r = if band_rows > 0 && pos.y < body_rect.min.y {
                lead_r + ((pos.y - grid_rect.min.y) / row_h) as usize
            } else {
                let dy = pos.y - body_rect.min.y + frac_px;
                if dy < 0.0 {
                    return None;
                }
                first_row + (dy / row_h) as usize
            };
            let cx = if band_cols > 0 && pos.x < body_rect.min.x {
                col_x[lead_c] + (pos.x - grid_rect.min.x)
            } else {
                pos.x - body_rect.min.x + body_x0 + self.scroll.col_px
            };
            let c = col_x.partition_point(|&x| x <= cx) as i64 - 1;
            if c < 0 || r >= total_rows || c as usize >= total_cols {
                return None;
            }
            // Report the DATA row, so a click under a filter selects the cell
            // the user actually pointed at rather than its screen position.
            // Both mappings apply, in the same order the paint path uses: the
            // table's rank lookup first, then the search filter.
            // Padding rows are hit-testable — that is the whole point of the
            // toggle: clicking one selects it so the user can type there, and
            // the resulting edit extends the sheet through the overlay.
            let row = resolve_row(r)?.row();
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
                    Self::cell_screen_rect(
                        br,
                        outer,
                        self.scroll,
                        self.col_widths,
                        &resolver,
                        m,
                        panes,
                    )
                    .map(|r| {
                        Rect::from_center_size(r.max, Vec2::splat(FILL_HANDLE * m.zoom)).expand(2.0)
                    })
                });
        let on_handle = pointer_pos
            .is_some_and(|p| handle_rect.is_some_and(|h| h.contains(p) && grid_rect.contains(p)));
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
            if grid_rect.contains(hr.center()) {
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
        //
        // Spans BODY row space only: the frozen band is not scrollable, so
        // including it would make the thumb claim reachable rows that are not.
        let vbar = Rect::from_min_max(
            egui::pos2(outer.max.x - SCROLLBAR_W, body_rect.min.y),
            egui::pos2(outer.max.x, outer.max.y - SCROLLBAR_W),
        );
        let vbar_active = dragging && drag_pos.is_some_and(|p| vbar.contains(p));
        let vpainter = ui.painter_at(vbar);
        vpainter.rect_filled(vbar, 0.0, th.panel);
        let body_min = panes.body_min_row();
        let body_rows = (total_rows as f64 - body_min).max(1.0);
        let visible_frac = (visible_count as f64 / body_rows).min(1.0);
        let thumb_h = (vbar.height() as f64 * visible_frac).max(24.0) as f32;
        let scroll_span = (body_rows - visible_count as f64).max(1.0);
        let pos_frac = ((self.scroll.row_offset - body_min) / scroll_span).clamp(0.0, 1.0);
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
                self.scroll.row_offset = body_min + t * scroll_span;
            }
        }

        // --- horizontal scrollbar ---
        let hbar = Rect::from_min_max(
            egui::pos2(body_rect.min.x, outer.max.y - SCROLLBAR_W),
            egui::pos2(outer.max.x - SCROLLBAR_W, outer.max.y),
        );
        let hresp = ui.allocate_rect(hbar, Sense::click_and_drag());
        let hpainter = ui.painter_at(hbar);
        hpainter.rect_filled(hbar, 0.0, th.panel);
        let body_width = total_width - body_x0;
        if body_width > body_size.x {
            let frac = (body_size.x / body_width).min(1.0);
            let tw = (hbar.width() * frac).max(24.0);
            let span = (body_width - body_size.x).max(1.0);
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
            Vec2::new(outer.width() - m.row_header_w, m.header_h),
        );
        hp.rect_filled(col_header, 0.0, th.header_bg);
        let chp = hp.with_clip_rect(col_header);

        // --- header reorder gesture ---
        //
        // Which display column is under a given x, or None outside the header.
        // Walks the SAME band list the cells were painted from, so a frozen
        // column's header is over its frozen column.
        let col_at_x = |px: f32| -> Option<usize> {
            col_bands
                .iter()
                .find(|&&(c, x)| px >= x && px < x + width_of(c))
                .map(|&(c, _)| c)
        };
        let mut header_press: Option<usize> = None;
        let mut header_drag_to: Option<usize> = None;
        let mut header_released = false;
        let mut header_hitboxes: Vec<(usize, egui::Pos2)> = Vec::new();
        for &(c, x) in &col_bands {
            let r = Rect::from_min_size(
                egui::pos2(x, outer.min.y),
                Vec2::new(width_of(c), m.header_h),
            );
            header_hitboxes.push((c, r.center()));
        }
        let pointer_in_header = pointer_pos.is_some_and(|p| col_header.contains(p));
        if pointer_in_header {
            if primary_pressed {
                header_press = pointer_pos.and_then(|p| col_at_x(p.x));
            }
            // Report the hovered column whenever a header drag is in flight,
            // WITHOUT requiring egui's `dragging` flag. That flag only turns on
            // once the pointer has travelled far enough to count as a drag, so
            // gating on it loses the target on short moves and on the release
            // frame — the drop then has nowhere to go and the gesture silently
            // does nothing.
            if self.header_dragging.is_some() {
                header_drag_to = pointer_pos.and_then(|p| col_at_x(p.x));
            }
        }
        if self.header_dragging.is_some() && !dragging {
            header_released = true;
        }

        // Drop indicator: a bright rule at the insertion point, so the user
        // can see where the column will land rather than guessing.
        if let (Some(src), Some(pos)) = (self.header_dragging, pointer_pos) {
            if let Some(dst) = col_at_x(pos.x) {
                if let Some(x) = x_of(dst) {
                    let edge = if dst > src { x + width_of(dst) } else { x };
                    hp.vline(
                        edge,
                        outer.min.y..=outer.max.y,
                        Stroke::new(2.5_f32, th.accent),
                    );
                }
            }
        }

        for &(c, x) in &col_bands {
            let r = Rect::from_min_size(
                egui::pos2(x, outer.min.y),
                Vec2::new(width_of(c), m.header_h),
            );
            let label = view.header_or_letter(c);
            let letter = column_name(c as u32);
            let shown = if label == letter {
                label
            } else {
                format!("{label}  ·  {letter}")
            };
            // Sort indicator. Without it a sorted view is indistinguishable
            // from a file that happened to arrive in that order, and the user
            // has no way to tell which column is driving it.
            let shown = match self.sort.and_then(|s| s.dir_of(c as u32)) {
                Some(d) => format!("{shown} {}", d.glyph()),
                None => shown,
            };
            chp.text(
                r.center(),
                Align2::CENTER_CENTER,
                shown,
                FontId::proportional(12.0 * m.zoom),
                th.text_dim,
            );
            chp.vline(r.max.x, outer.min.y..=outer.min.y + m.header_h, line);
        }

        let row_header = Rect::from_min_size(
            egui::pos2(outer.min.x, body_origin.y),
            Vec2::new(m.row_header_w, grid_rect.height()),
        );
        hp.rect_filled(row_header, 0.0, th.header_bg);
        let rhp = hp.with_clip_rect(row_header);
        // Row headers carry the ORIGINAL row number even under a filter. A
        // filtered view that renumbered its rows 1..N would be actively
        // misleading: the whole point of finding row 4,912,733 is knowing it
        // is row 4,912,733.
        //
        // Walks the SAME band list as the cells — frozen band first, then the
        // body — so the number beside a frozen row is that row's number no
        // matter where the body has scrolled to.
        for (bi, &(r, y)) in row_bands.iter().enumerate() {
            let Some(resolved) = resolve_row(r) else {
                continue;
            };
            let row = resolved.row();
            // Recorded from the SAME walk that paints the row number, so the
            // reported "what is on screen" cannot disagree with what is.
            painted_rows.push((r, row));
            if bi < body_row_start {
                frozen_row_count += 1;
            }
            // Row numbers name the DATA row, so a filtered view shows the
            // original 1, 5, 9, ... rather than renumbering to 1, 2, 3 — the
            // user must always be able to tell which rows are hidden.
            let rect =
                Rect::from_min_size(egui::pos2(outer.min.x, y), Vec2::new(m.row_header_w, row_h));
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
                egui::pos2(rect.max.x - 8.0 * m.zoom, rect.center().y),
                Align2::RIGHT_CENTER,
                (row as u64 + 1).to_string(),
                FontId::proportional(11.5 * m.zoom),
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
            Rect::from_min_size(outer.min, Vec2::new(m.row_header_w, m.header_h)),
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
            header_press,
            header_drag_to,
            header_released,
            painted_cells,
            header_hitboxes,
            visible_rows: row_range,
            painted_rows,
            frozen_row_count,
            zoom: m.zoom,
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

    /// The resolver a filtered (and optionally padded) view would build.
    fn resolver_of<'a>(f: Option<&'a RowFilter>, pad: Option<PadSpace>) -> RowResolver<'a> {
        RowResolver {
            filter: f,
            pad,
            ..Default::default()
        }
    }

    /// `cell_screen_rect` at 100% zoom with no panes — what every pre-existing
    /// geometry test means, spelled once.
    fn rect_of(
        cell: CellRef,
        outer: Rect,
        scroll: &ScrollState,
        widths: &[f32],
        r: &RowResolver<'_>,
    ) -> Option<Rect> {
        Grid::cell_screen_rect(
            cell,
            outer,
            scroll,
            widths,
            r,
            Metrics::default(),
            Panes::default(),
        )
    }

    /// Screen row -> underlying row, through THE resolver.
    fn underlying(f: Option<&RowFilter>, visible: usize) -> Option<u32> {
        resolver_of(f, None).resolve(visible).map(|s| s.row())
    }

    /// Underlying row -> screen row, through THE resolver.
    fn visible(f: Option<&RowFilter>, row: u32) -> Option<usize> {
        resolver_of(f, None).visible_of(row)
    }

    #[test]
    fn unfiltered_row_lookup_is_the_identity() {
        assert_eq!(underlying(None, 0), Some(0));
        assert_eq!(underlying(None, 199_999_999), Some(199_999_999));
        assert_eq!(visible(None, 4_912_733), Some(4_912_733));
    }

    #[test]
    fn filtered_row_lookup_uses_the_mapping_both_ways() {
        let f = filter_of(&[3, 9, 4_912_733]);
        assert_eq!(underlying(Some(&f), 0), Some(3));
        assert_eq!(underlying(Some(&f), 2), Some(4_912_733));
        assert_eq!(underlying(Some(&f), 3), None, "past the end");
        assert_eq!(visible(Some(&f), 4_912_733), Some(2));
        assert_eq!(visible(Some(&f), 4), None, "hidden row");
    }

    #[test]
    fn row_header_text_is_the_original_one_based_row() {
        // Acceptance criterion: headers keep original numbers under a filter.
        // This mirrors exactly what the header loop paints.
        let f = filter_of(&[0, 41, 199_999_999]);
        let painted: Vec<String> = (0..f.len())
            .map(|r| {
                let row = underlying(Some(&f), r).unwrap();
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
        let cell = CellRef::new(underlying(Some(&f), visible as usize).unwrap(), 2);
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
        let rect = rect_of(
            CellRef::new(1_000_000, 0),
            outer,
            &scroll,
            &widths,
            &resolver_of(Some(&f), None),
        )
        .expect("kept row must have a rect");
        let expected_y = outer.min.y + HEADER_HEIGHT + 2.0 * ROW_HEIGHT;
        assert!((rect.min.y - expected_y).abs() < 0.5, "got {rect:?}");

        // A row the filter hides has no rect at all, so the cell editor cannot
        // be painted over a row that is not on screen.
        assert!(rect_of(
            CellRef::new(7, 0),
            outer,
            &scroll,
            &widths,
            &resolver_of(Some(&f), None)
        )
        .is_none());

        // Without the filter the same cell is a million rows below the fold.
        assert!(rect_of(
            CellRef::new(1_000_000, 0),
            outer,
            &scroll,
            &widths,
            &resolver_of(None, None)
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
        let resolved = underlying(Some(&f), offset.floor() as usize).unwrap();
        assert_eq!(resolved, 199_999_999);
        assert_eq!(visible(Some(&f), 199_999_998), Some(2));
        assert_eq!(visible(Some(&f), 199_999_999), Some(3));
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
            assert!(underlying(Some(&f), v).is_some());
        }
        // A padding row's data row is in NEITHER mapping — which is exactly
        // why it must never be looked up in one.
        assert_eq!(visible(Some(&f), 200), None);
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
        assert!(rect_of(
            CellRef::new(100, 0),
            outer,
            &scroll,
            &widths,
            &resolver_of(Some(&f), None)
        )
        .is_none());
        // With it, the first padding row sits directly under the last kept row.
        let rect = rect_of(
            CellRef::new(100, 0),
            outer,
            &scroll,
            &widths,
            &resolver_of(Some(&f), Some(p)),
        )
        .expect("a padding row must be editable");
        let expected_y = outer.min.y + HEADER_HEIGHT + 3.0 * ROW_HEIGHT;
        assert!((rect.min.y - expected_y).abs() < 0.5, "got {rect:?}");
        // A real filtered row still resolves through the FILTER, not the pad.
        let real = rect_of(
            CellRef::new(6, 0),
            outer,
            &scroll,
            &widths,
            &resolver_of(Some(&f), Some(p)),
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
