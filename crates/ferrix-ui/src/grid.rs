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
use ferrix_core::sizing::{HiddenRows, Outline};
use ferrix_core::subtotal::{SubtotalCell, SubtotalPlan};
use ferrix_core::{column_name, CellRef, RowFilter, Selection, SortOrder, Value};

use std::collections::BTreeSet;

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

/// Columns a sheet with NO DATA offers anyway (issue #52).
///
/// A brand-new sheet holds nothing, so `view.col_count()` is 0 — and a grid
/// zero columns wide has nowhere to put a cursor and nothing to hit-test, so
/// every click and keystroke lands nowhere. A blank spreadsheet page is
/// A..Z, so that is what an empty sheet offers.
///
/// Like [`EMPTY_ROW_PADDING`], this is PURE VIEWPORT: it widens what can be
/// scrolled to and typed in, and `SheetView::col_count` — what export and the
/// status bar read — never sees it. Typing in one of these columns extends
/// the sheet through the overlay's own extent, exactly as typing in a padding
/// row does.
pub const BLANK_SHEET_COLS: usize = 26;

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
    /// A SYNTHETIC subtotal row (issue #34). Not a row of the sheet at all:
    /// it is drawn from the group aggregates the plan already computed, it
    /// holds no cell, and it is in no mapping below this stage. The payload
    /// is the group index.
    ///
    /// `row()` reports the last DATA row of the group, so a caller that only
    /// wants "roughly where am I" — scroll anchoring, the selection's row
    /// range — gets a sane answer, while anything that must distinguish the
    /// two matches on the variant.
    Subtotal { group: usize, last_row: u32 },
}

impl ScreenRow {
    #[inline]
    pub fn row(self) -> u32 {
        match self {
            ScreenRow::Data(r) | ScreenRow::Pad(r) => r,
            ScreenRow::Subtotal { last_row, .. } => last_row,
        }
    }

    #[inline]
    pub fn is_pad(self) -> bool {
        matches!(self, ScreenRow::Pad(_))
    }

    /// True for a synthetic subtotal row. Callers that write, edit or export
    /// must check this: a subtotal row is not a cell the user can type into,
    /// and treating it as one would write into whatever row the mappings
    /// below happen to resolve.
    #[inline]
    pub fn is_subtotal(self) -> bool {
        matches!(self, ScreenRow::Subtotal { .. })
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
    /// Rows hidden by a zero height or a collapsed outline group (issue #29).
    ///
    /// A STAGE OF THIS RESOLVER, not a lookup the painter does for itself.
    /// Hiding narrows the screen index before the filter/sort mappings see
    /// it, exactly as the table filter narrows before the search filter: the
    /// paint loop asks for screen row N and gets the Nth row that survives
    /// every transform, so no caller can pair a hidden-row test with a row
    /// number that came from a different mapping.
    ///
    /// Indexed in the space the mappings below it consume — see
    /// [`HiddenRows`] and `FerrixApp::hidden_index`, which projects underlying
    /// spans into view space when a sort or filter is also active.
    pub hidden: Option<&'a HiddenRows>,
    /// Subtotal rows inserted at each change of a grouped column's value
    /// (issue #34).
    ///
    /// A STAGE OF THIS RESOLVER, exactly like `hidden`, and for the same
    /// reason: a second mapping consulted alongside this one would let a
    /// caller pair a subtotal test with a row number from a different
    /// transform, which is how this repo once painted wrong records under
    /// correct row numbers.
    ///
    /// It sits ABOVE the sort/filter mappings — its input is a VISIBLE
    /// position, not a data row — so sort and filter keep working untouched
    /// underneath it, and dropping this field restores the exact original
    /// view.
    pub subtotals: Option<&'a SubtotalPlan>,
}

impl<'a> RowResolver<'a> {
    /// Screen row -> the row it actually shows.
    #[inline]
    pub fn resolve(&self, r: usize) -> Option<ScreenRow> {
        if let Some(row) = self.pad.and_then(|p| p.data_row(r)) {
            return Some(ScreenRow::Pad(row));
        }
        // Subtotals are applied FIRST among the real transforms, because they
        // ADD screen rows: "the Nth row on screen" has to lose the synthetic
        // rows before it means anything to the mappings below, exactly as
        // hiding has to remove the hidden ones. A subtotal row short-circuits
        // — it is in no mapping below this point, and feeding its index to
        // one would alias onto an unrelated record.
        let r = match self.subtotals {
            Some(p) if !p.is_empty() => match p.resolve(r)? {
                ferrix_core::SubRow::Data(v) => v,
                ferrix_core::SubRow::Subtotal(g) => {
                    // The group's last DATA row, resolved through the rest of
                    // the stack so `row()` names a real record even under a
                    // sort. One call, on the same path everything else uses.
                    let last_row = p
                        .groups()
                        .get(g)
                        .and_then(|grp| self.resolve_below(grp.last))
                        .unwrap_or(0);
                    return Some(ScreenRow::Subtotal { group: g, last_row });
                }
            },
            _ => r,
        };
        self.resolve_below(r).map(ScreenRow::Data)
    }

    /// The stages below subtotals: hiding, then the filter/sort mappings.
    ///
    /// Split out so the subtotal stage can ask "what data row is visible
    /// position N" through the SAME code the data path uses, rather than
    /// re-deriving it.
    #[inline]
    fn resolve_below(&self, r: usize) -> Option<u32> {
        // Hiding is applied FIRST among the mapping transforms: it turns "the
        // Nth row on screen" into "the Nth row that is not hidden", which is
        // the index the filter and sort mappings are built over. Applying it
        // after them would test a hidden row against an already-mapped index
        // and skip the wrong record.
        let r = match self.hidden {
            Some(h) if !h.is_empty() => h.nth_visible(r),
            _ => r,
        };
        // Sort is composed AFTER the filters: its candidate set was the rows
        // they kept, so its values are underlying rows and asking a filter
        // again would map an already-mapped index a second time.
        if let Some(s) = self.sort {
            return s.underlying(r);
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
        // Mirror of `resolve`, in reverse order: undo the mappings first, then
        // undo the hiding, then re-add the subtotal rows. A hidden row has no
        // screen position at all, which is what stops the in-cell editor
        // drawing over the row that took its place.
        let pre_hide = if let Some(s) = self.sort {
            s.visible_of(row)?
        } else {
            match self.filter {
                Some(f) => f.visible_of(row)?,
                None => row as usize,
            }
        };
        let pre_sub = match self.hidden {
            Some(h) if !h.is_empty() => h.visible_index(pre_hide as u32)?,
            _ => pre_hide,
        };
        match self.subtotals {
            Some(p) if !p.is_empty() => p.screen_of(pre_sub),
            _ => Some(pre_sub),
        }
    }

    /// Rows the view transforms resolve, before padding is added on top.
    pub fn resolved_rows(&self, data_rows: usize) -> usize {
        let mapped = if let Some(s) = self.sort {
            s.len()
        } else {
            match self.filter {
                Some(f) => f.len(),
                None => match self.table {
                    Some(t) => t.visible_row_count(data_rows),
                    None => data_rows,
                },
            }
        };
        // Hidden rows shorten the view, so the scrollable extent — and with it
        // where the empty-row padding starts — has to shrink by the same
        // amount. Otherwise the last screen rows resolve past the end.
        let visible = match self.hidden {
            Some(h) if !h.is_empty() => h.visible_count(mapped),
            _ => mapped,
        };
        // Subtotals LENGTHEN it, by one synthetic row per group.
        match self.subtotals {
            Some(p) if !p.is_empty() => visible + p.groups().len(),
            _ => visible,
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
    /// Cell whose comment marker the pointer is resting on, if any. The caller
    /// paints the tooltip, because the grid draws onto a raw `Painter` and has
    /// no widget `Response` to hang egui's hover machinery off.
    pub hovered_comment: Option<CellRef>,
    /// Cell the pointer was over when the SECONDARY button was clicked, and
    /// the screen point it happened at. The caller opens its context menu
    /// there — on the cell the user aimed at, not on wherever the selection
    /// happened to be.
    pub context_click: Option<(CellRef, egui::Pos2)>,
    /// How many comment markers were painted this frame. Real paint output, so
    /// a test asserting "the marker is gone" reads the screen rather than the
    /// model.
    pub comment_markers: usize,
    /// Column whose RIGHT BORDER was pressed, with the pointer x and the
    /// column's current width. Reported on PRESS — see [`GridResponse::header_press`]
    /// for why keying a gesture off a click (which fires on release) makes it
    /// never start.
    pub resize_started: Option<(usize, f32, f32)>,
    /// Column and the width the in-flight resize drag currently implies.
    ///
    /// Reported from the app's OWN press/release state, never from egui's
    /// `is_dragging`: that flag only turns on past a movement threshold and is
    /// cleared on the release frame, so the target would vanish exactly when
    /// the drop needs it.
    pub resize_to: Option<(usize, f32)>,
    /// Set on the frame a resize drag is released.
    pub resize_released: bool,
    /// Column whose border was double-clicked — the autofit gesture.
    pub col_autofit: Option<usize>,
    /// Header that was right-clicked, and where, for the hide/unhide menu.
    pub header_context: Option<(usize, egui::Pos2)>,
    /// Display ROW whose header was pressed (issue #17).
    ///
    /// Reported on PRESS for the same reason [`GridResponse::header_press`] is:
    /// `primary_clicked` fires on RELEASE, so a gesture keyed off it can never
    /// start a drag and — more importantly here — a press-and-hold would leave
    /// the row unselected until the button came back up.
    ///
    /// Carries the modifiers as seen on THIS frame, because the caller needs
    /// to know whether the press was plain, Ctrl (add a disjoint row) or Shift
    /// (extend a span), and egui's aggregate `i.modifiers` is only correct
    /// while the frame that produced the press is still being handled.
    pub row_header_press: Option<(u32, egui::Modifiers)>,
    /// Where each visible row's header was actually painted this frame:
    /// (display row, centre point).
    ///
    /// Same contract as [`GridResponse::header_hitboxes`], and it exists for
    /// the same reason: the row band's y depends on the scroll offset, the
    /// zoom, per-row heights and the frozen band, so a test that hard-codes
    /// pixels tests arithmetic rather than the gesture.
    pub row_header_hitboxes: Vec<(u32, egui::Pos2)>,
    /// Outline group whose expand/collapse button was clicked, named by the
    /// group's first row.
    pub outline_toggle: Option<u32>,
    /// Outline toggle buttons painted this frame. Real paint output, so a test
    /// can assert the gutter actually drew a control rather than trusting the
    /// model.
    pub outline_buttons: usize,
    /// SUBTOTAL rows painted this frame, and the aggregate texts drawn on
    /// them (issue #34).
    ///
    /// Real paint output, counted where they are drawn. `paint_text_count()`
    /// cannot answer "did the subtotal actually render" — it moves whenever
    /// any cell does — so these are counted separately and a test can assert
    /// that removing the grouping takes them back to zero.
    pub subtotal_rows: usize,
    pub subtotal_texts: usize,
    /// Border EDGES drawn this frame (issue #28).
    ///
    /// Real paint output, counted at the point of drawing, and counted once
    /// per edge rather than once per stroke — so a shared edge between two
    /// bordered neighbours contributes 1 and a test can prove it is not
    /// double-drawn. `paint_shape_count` cannot answer that: a double border
    /// emits two strokes and a dashed one emits many, so a total would move
    /// for reasons unrelated to the property being asserted.
    pub border_segments: usize,
    /// Cells whose text was painted ROTATED this frame.
    ///
    /// A rotated cell emits an `egui::Shape::Text` with a non-zero angle
    /// instead of a plain galley — a different shape, from a different call.
    /// Counted here so a test can assert rotation actually reached the
    /// painter rather than inspecting pixels.
    pub rotated_texts: usize,
    /// Cells whose text was laid out WRAPPED this frame.
    pub wrapped_texts: usize,
    /// The in-cell dropdown arrow's hit rectangle, when one was drawn this
    /// frame (issue #41). Real paint geometry, reported so the caller can
    /// open the list on a click at the arrow rather than guessing where it is.
    pub dropdown_button: Option<(CellRef, egui::Rect)>,
    /// Sparkline PRIMITIVES drawn this frame (issue #36): one per line
    /// segment, one per bar.
    ///
    /// Counted at the point of drawing, and counted as the specific shape kind
    /// this feature emits rather than as a slice of the frame total. A total
    /// moves for a dozen unrelated reasons — a selection rectangle, one fewer
    /// grid line — so "sparklines are painted" asserted against
    /// `paint_shape_count()` alone would be a test of the whole frame. This
    /// number is zero on every sheet without a sparkline group and is exactly
    /// the count of marks the feature put on screen.
    pub sparkline_shapes: usize,
    /// Cells a sparkline group covers that drew NOTHING this frame, because
    /// their source was empty or held no numbers.
    ///
    /// Separate from `sparkline_shapes` so "draws nothing rather than
    /// erroring" is an assertable state and not merely the absence of one.
    pub sparkline_blanks: usize,
}

pub struct Grid<'a> {
    pub view: &'a SheetView<'a>,
    /// The active selection. Its cursor is drawn with a strong border, the
    /// rest of the range with a translucent fill.
    pub selection: Option<Selection>,
    /// Additional, DISJOINT selected ranges (issue #17).
    ///
    /// Ctrl+clicking a second row or column adds a range here rather than
    /// growing `selection` into a bounding box that would cover everything
    /// between them. Each is still a pair of corners, so selecting rows 1 and
    /// 50,000,000 of a 200M-row sheet costs 32 bytes, not a row list.
    ///
    /// Kept as a slice the caller owns: the grid paints them and has no
    /// opinion about how they were accumulated.
    pub extra_selections: &'a [Selection],
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
    /// Columns the grid offers when the sheet's BASE has none (issue #52).
    ///
    /// Zero means "use the view's own count" — the caller sets it, so the
    /// grid and the app's hit-testing/navigation cannot disagree about how
    /// wide a blank sheet is.
    pub blank_cols: usize,
    /// Sheet-wide formatting: manual colours and type styling that apply to
    /// any cell, table or not. `None` when nothing has been formatted, which
    /// keeps the default path free of lookups.
    pub format: Option<&'a ferrix_core::SheetFormat>,
    /// Merged regions. `None` when the sheet has none, keeping the default
    /// paint path free of lookups.
    pub merges: Option<&'a ferrix_core::merge::MergeMap>,
    /// Cell comments, for the corner marker and the hover tooltip.
    ///
    /// Consulted once per visible ROW, not once per visible cell: the paint
    /// loop hoists `row_comments()` out of its column loop, and a sheet with
    /// no comments at all short-circuits on `is_empty()` before touching the
    /// map. That is what keeps a 200M-row sheet's frame cost unchanged by
    /// this feature.
    pub comments: Option<&'a ferrix_core::CommentMap>,
    /// Active sort, when a column header has been clicked. A VIEW TRANSFORM:
    /// it permutes which underlying row each screen row shows and never moves
    /// a byte of data. Composed after the filters through [`RowResolver`].
    pub sort: Option<&'a ferrix_core::SortOrder>,
    /// Active subtotal grouping (issue #34). A VIEW TRANSFORM in exactly the
    /// shape `sort` is: nothing is inserted into the sheet, and it composes
    /// through [`RowResolver`] rather than beside it.
    pub subtotals: Option<&'a SubtotalPlan>,
    /// Zoom-scaled lengths. Everything the grid draws is sized from this, so
    /// zoom is one multiply at the layout level rather than a special case in
    /// every paint call.
    pub metrics: Metrics,
    /// Frozen / split leading band. Mutable because a SPLIT band scrolls
    /// independently, and the wheel over the band is what moves it.
    pub panes: &'a mut Panes,
    /// Columns hidden explicitly or by a collapsed column group (issue #29).
    ///
    /// Consumed through `width_of`, which reports a hidden column as ZERO
    /// WIDE. Paint and hit-testing both derive from that one function, so a
    /// hidden column cannot be skipped by one and hit by the other.
    pub hidden_cols: Option<&'a BTreeSet<u32>>,
    /// Rows hidden by a zero height or a collapsed group. Composed into
    /// [`RowResolver`] as a stage — never consulted directly by the painter.
    pub hidden_rows: Option<&'a HiddenRows>,
    /// Row outline groups, for the expand/collapse gutter.
    pub row_outline: Option<&'a Outline>,
    /// Column currently being resized by its border, with the pointer x the
    /// drag started at and the width it started from. Owned by the APP between
    /// press and release, so the gesture survives the release frame.
    pub col_resizing: Option<(usize, f32, f32)>,
    /// Sheet-range data validation (issue #41), for the in-cell dropdown
    /// affordance.
    ///
    /// Consulted for ONE cell per frame — the selection cursor — the way
    /// Excel draws it: the arrow appears on the active cell, not on every cell
    /// in the range. That is what keeps a list rule over 200M rows from
    /// costing a lookup per painted cell.
    pub validation: Option<&'a ferrix_core::SheetValidation>,
    /// Show formula SOURCE instead of computed values (issue #38, Ctrl+`).
    ///
    /// A flag, not a precomputed map of text. The source is fetched with
    /// `view.edit_text(cell)` inside the paint loop, for the cells being
    /// painted and no others — so a 200M-row sheet in this mode costs exactly
    /// what a viewport of formula sources costs, and materialising every
    /// formula in the sheet never happens.
    pub show_formulas: bool,
    /// Sparkline groups (issue #36). `None` when the sheet has none, which
    /// keeps the default paint path free of lookups.
    ///
    /// There are NO chart objects behind this. Each visible cell a group
    /// covers reads its own row's source span, reduces it through
    /// `ferrix_core::sparkline_shape`, and draws — inside the cell loop, from
    /// the same `cell_rect` the value would have used. So the per-frame cost
    /// is (visible sparklined cells x source span), the source span is
    /// reduced to the cell's pixel width by `decimate_min_max`, and the row
    /// count of the sheet does not appear in that product at all.
    pub sparklines: Option<&'a ferrix_core::SparklineMap>,
}

fn sheet_c32(c: ferrix_core::Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c.0, c.1, c.2)
}

/// Draw one sparkline into `rect`, returning how many primitives it emitted.
///
/// Takes NORMALISED geometry from `ferrix_core::sparkline` and does nothing
/// but map it onto pixels. The split is deliberate: extents, decimation and
/// baseline choice are data questions and live in core beside `chart.rs`,
/// while this function owns only the affine map into the cell -- so a test of
/// what is drawn does not need a screen, and this function has no arithmetic
/// that could disagree with the model about where a point belongs.
///
/// Every mark is added straight to the frame's `Painter`. Nothing is retained
/// between frames, which is the whole reason there is no chart object.
fn paint_sparkline(
    painter: &egui::Painter,
    rect: Rect,
    shape: &ferrix_core::SparkShape,
    th: Theme,
    zoom: f32,
) -> usize {
    use ferrix_core::SparkShape;
    // y is measured UP from the bottom of the cell in the model and DOWN from
    // the top in egui, so the flip happens once, here.
    let px = |nx: f64, ny: f64| -> egui::Pos2 {
        egui::pos2(
            rect.min.x + rect.width() * nx as f32,
            rect.max.y - rect.height() * ny as f32,
        )
    };
    match shape {
        SparkShape::Line(points) => {
            let stroke = Stroke::new(1.0_f32.max(zoom), th.accent);
            if points.len() == 1 {
                // A single point would be an empty polyline and so invisible;
                // a lone reading is still a fact worth showing.
                let p = px(points[0].x, points[0].y);
                painter.circle_filled(p, 1.2 * zoom, th.accent);
                return 1;
            }
            let mut drawn = 0usize;
            for w in points.windows(2) {
                painter.line_segment([px(w[0].x, w[0].y), px(w[1].x, w[1].y)], stroke);
                drawn += 1;
            }
            drawn
        }
        SparkShape::Bars(bars) => {
            let mut drawn = 0usize;
            for b in bars {
                let top = px(b.x0, b.hi);
                let bottom = px(b.x1, b.lo);
                // A bar that rounds to sub-pixel height still has to be
                // visible: a win/loss column of tiny values must not silently
                // become an empty cell.
                let r = Rect::from_min_max(
                    egui::pos2(top.x, top.y),
                    egui::pos2(bottom.x.max(top.x + 1.0), bottom.y.max(top.y + 1.0)),
                );
                painter.rect_filled(r, 0.0, if b.negative { th.error } else { th.accent });
                drawn += 1;
            }
            drawn
        }
    }
}

/// THE row-height definition for wrapped text (issue #28).
///
/// Paint, the grid lines, the row-number gutter, the hit test and
/// [`Grid::cell_screen_rect`] all get a row's height from this one type. A
/// second, independently-derived height is exactly the failure the guide
/// warns about for row mappings: a wrapped row would be PAINTED 44px tall
/// and CLICKED as 22px tall, so a click near its bottom would select the row
/// below the one the user is looking at, and no single-feature test would see
/// it because each feature would be self-consistent.
///
/// Construction is O(configured wrap scopes), not O(rows) and not O(cells):
/// it resolves the set of WRAPPING COLUMNS once, and then measures only the
/// rows it is actually asked about.
pub struct RowHeights<'a> {
    format: Option<&'a ferrix_core::SheetFormat>,
    view: &'a SheetView<'a>,
    col_widths: &'a [f32],
    /// Columns some scope asked to wrap. EMPTY is the overwhelmingly common
    /// case and short-circuits every query to the uniform height.
    wrap_cols: Vec<u32>,
}

impl<'a> RowHeights<'a> {
    pub fn new(
        format: Option<&'a ferrix_core::SheetFormat>,
        view: &'a SheetView<'a>,
        col_widths: &'a [f32],
    ) -> Self {
        let mut wrap_cols = Vec::new();
        if let Some(f) = format.filter(|f| f.has_decor()) {
            let max_col = view.col_count().max(col_widths.len()).saturating_sub(1) as u32;
            f.wrapping_cols(max_col, &mut wrap_cols);
        }
        Self {
            format,
            view,
            col_widths,
            wrap_cols,
        }
    }

    /// Does every row have the same height? The fast path, and the state of
    /// any sheet that has never used wrap.
    #[inline]
    pub fn is_uniform(&self) -> bool {
        self.wrap_cols.is_empty()
    }

    /// Height of one DATA row, at the given metrics.
    ///
    /// Measured against UNZOOMED widths so the line count — and therefore how
    /// many rows fit on screen — does not change with zoom. Zoom scales the
    /// result, exactly as it scales the uniform height.
    pub fn height_of(&self, row: u32, m: Metrics) -> f32 {
        if self.is_uniform() {
            return m.row_h;
        }
        let Some(fmt) = self.format else {
            return m.row_h;
        };
        let mut lines = 1u32;
        let mut plan = Vec::new();
        for &c in &self.wrap_cols {
            fmt.decor_plan(c, &mut plan);
            let cref = CellRef::new(row, c);
            let d = fmt.resolve_decor(cref, &plan);
            if !d.wraps() {
                continue;
            }
            let text = self.view.display(cref);
            if text.is_empty() {
                continue;
            }
            let w = self
                .col_widths
                .get(c as usize)
                .copied()
                .unwrap_or(DEFAULT_COL_WIDTH);
            lines = lines.max(ferrix_core::format::wrapped_line_count(
                &text,
                w,
                d.indent_px(),
            ));
        }
        ferrix_core::format::wrapped_row_height(lines, ROW_HEIGHT) * m.zoom
    }

    /// Height of a SCREEN row, resolved through the one row resolver.
    ///
    /// A padding row is past the end of the sheet and holds no text, so it is
    /// always the uniform height.
    pub fn screen_height(&self, r: usize, resolver: &RowResolver<'_>, m: Metrics) -> f32 {
        if self.is_uniform() {
            return m.row_h;
        }
        match resolver.resolve(r) {
            Some(sr) if !sr.is_pad() => self.height_of(sr.row(), m),
            _ => m.row_h,
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
        resolver: &RowResolver<'_>,
        m: Metrics,
        panes: Panes,
    ) -> Option<Rect> {
        Self::cell_screen_rect_h(cell, outer, scroll, col_widths, resolver, m, panes, None)
    }

    /// [`Grid::cell_screen_rect`] with wrapped-row heights taken into account.
    ///
    /// `heights` is `None` for callers that have no `SheetFormat` to hand, in
    /// which case every row is the uniform height — which is also exactly
    /// what [`RowHeights::is_uniform`] reports for a sheet that has never
    /// used wrap, so the two paths agree on every sheet without wrapping.
    #[allow(clippy::too_many_arguments)]
    pub fn cell_screen_rect_h(
        cell: CellRef,
        outer: Rect,
        scroll: &ScrollState,
        col_widths: &[f32],
        resolver: &RowResolver<'_>,
        m: Metrics,
        panes: Panes,
        heights: Option<&RowHeights<'_>>,
    ) -> Option<Rect> {
        let body_origin = outer.min + Vec2::new(m.row_header_w, m.header_h);
        // Through THE resolver, so the editor lands on the same screen row the
        // paint loop drew the cell on — under a filter, a sort, or both.
        let visible = resolver.visible_of(cell.row)?;
        let w_of = |c: usize| -> f32 {
            m.col_width(col_widths.get(c).copied().unwrap_or(DEFAULT_COL_WIDTH))
        };
        // Height of a SCREEN row, from the same source the paint loop uses.
        // Falls back to the uniform height when the caller has no format.
        let h_of =
            |r: usize| -> f32 { heights.map_or(m.row_h, |h| h.screen_height(r, resolver, m)) };
        let uniform = heights.is_none_or(|h| h.is_uniform());

        // Band extents, mirroring `show`. Widths and heights come from the
        // SAME metrics and the SAME col_widths the body uses — a frozen column
        // is the same column, not a copy of it.
        let band_rows = panes.rows;
        let band_cols = panes.cols;
        // A band of wrapped rows is as tall as its rows actually are, summed
        // through the same function that paints them.
        let band_h: f32 = if uniform {
            band_rows as f32 * m.row_h
        } else {
            (panes.lead_first_row()..panes.lead_first_row() + band_rows)
                .map(h_of)
                .sum()
        };
        let lead_r = panes.lead_first_row();
        let lead_c = panes.lead_first_col();
        let band_w: f32 = (lead_c..lead_c + band_cols).map(w_of).sum();

        let this_h = h_of(visible);
        // A row inside the leading band is painted from the band's own offset,
        // which under a freeze never moves — so it has a rect no matter how
        // far the body has scrolled.
        let y = if band_rows > 0 && visible >= lead_r && visible < lead_r + band_rows {
            if uniform {
                body_origin.y + (visible - lead_r) as f32 * m.row_h
            } else {
                body_origin.y + (lead_r..visible).map(h_of).sum::<f32>()
            }
        } else {
            let first = scroll.row_offset.floor().max(0.0) as usize;
            let frac_px = ((scroll.row_offset - first as f64) * m.row_h as f64) as f32;
            let yy = if uniform {
                let rel = visible as f64 - scroll.row_offset;
                body_origin.y + band_h + (rel * m.row_h as f64) as f32
            } else if visible >= first {
                // Running sum from the first body row, exactly the way `show`
                // lays the body band out. Bounded by the viewport: the loop
                // below stops as soon as it has left the visible area.
                let mut acc = body_origin.y + band_h - frac_px;
                for r in first..visible {
                    acc += h_of(r);
                    if acc > outer.max.y {
                        return None;
                    }
                }
                acc
            } else {
                // Above the viewport: not painted, so it has no rect. Summing
                // backwards would invent one for a row `show` never drew.
                return None;
            };
            if yy + this_h < body_origin.y + band_h || yy > outer.max.y {
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
        Some(Rect::from_min_size(egui::pos2(x, y), Vec2::new(w, this_h)))
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
            hidden: self.hidden_rows,
            subtotals: self.subtotals,
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
        // A sheet whose BASE has no columns still gets a page to type into
        // (issue #52), supplied by the caller so this and the app's
        // hit-testing agree by construction. `max` rather than a replacement:
        // once the overlay has widened the view past the blank page, the
        // view's own count wins.
        let total_cols = view.col_count().max(self.blank_cols).max(1);

        // THE single row resolution, used by painting, row headers,
        // hit-testing and the cell editor alike. Filters compose first, sort
        // last, padding short-circuits everything — see [`RowResolver`].
        let resolver = RowResolver { pad, ..unpadded };
        let resolve_row = |r: usize| -> Option<ScreenRow> { resolver.resolve(r) };

        let m = self.metrics;
        let row_h = m.row_h;
        // THE column width. A hidden column is ZERO WIDE rather than absent
        // from a separate list, so the prefix sum, the paint loop, the header
        // band and the hit test all agree by construction: a zero-wide column
        // occupies no pixels to paint into and no pixels to click on, and its
        // NEIGHBOUR keeps the x range the hidden column gave up. A parallel
        // "visible columns" vector would have needed every one of those call
        // sites to consult it, and the one that forgot would hit-test a
        // column the user cannot see.
        let hidden_cols = self.hidden_cols;
        let width_of = |c: usize| -> f32 {
            if hidden_cols.is_some_and(|h| h.contains(&(c as u32))) {
                return 0.0;
            }
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
        // A click that egui has already given to a floating window (a modal,
        // a menu, a combo popup) must NOT also reach the grid underneath.
        //
        // The grid hand-hit-tests raw pointer state rather than going through
        // a widget Response, so without this check pressing "OK" in a dialog
        // ALSO lands on whatever cell happens to be behind the button — which
        // silently collapses the user's selection at the exact moment they
        // are acting on it. That is how the conditional-format editor's second
        // rule ended up on a different range than its first.
        //
        // Secondary clicks are gated too: a right-click on the comment context
        // menu must not simultaneously re-target the cell beneath it.
        let over_window = ui.ctx().is_pointer_over_area() && !ui.ui_contains_pointer();
        let (
            pointer_pos,
            wheel,
            primary_clicked,
            primary_pressed,
            primary_double,
            dragging,
            secondary_clicked,
        ) = ui.ctx().input(|i| {
            (
                i.pointer.interact_pos().filter(|_| !over_window),
                if over_window {
                    egui::Vec2::ZERO
                } else {
                    i.raw_scroll_delta
                },
                i.pointer.primary_clicked() && !over_window,
                i.pointer.primary_pressed() && !over_window,
                i.pointer
                    .button_double_clicked(egui::PointerButton::Primary)
                    && !over_window,
                i.pointer.primary_down() && !over_window,
                i.pointer.button_clicked(egui::PointerButton::Secondary) && !over_window,
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
        // Columns are built FIRST because wrapped-text row heights (issue
        // #28) need to know which columns are on screen and how wide they
        // are: a row is only as tall as the visible cell that wraps in it.
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

        // --- wrapped-text row heights (issue #28) ---
        //
        // Decoration plans, built once per VISIBLE COLUMN per frame exactly
        // like the conditional-format plans below, and only for columns that
        // can wrap at all. A sheet with no decoration configured short-
        // circuits on `has_decor()` and does no work here whatsoever, so this
        // feature costs an undecorated 200M-row sheet one boolean per frame.
        let decor_plans: Vec<(usize, Vec<ferrix_core::DecorEntry>)> =
            match self.format.filter(|f| f.has_decor()) {
                Some(fmt) => col_bands
                    .iter()
                    .map(|&(c, _)| {
                        let mut p = Vec::new();
                        fmt.decor_plan(c as u32, &mut p);
                        (c, p)
                    })
                    .filter(|(_, p)| !p.is_empty())
                    .collect(),
                None => Vec::new(),
            };
        let decor_plan_of = |c: usize| -> Option<&[ferrix_core::DecorEntry]> {
            decor_plans
                .iter()
                .find(|(cc, _)| *cc == c)
                .map(|(_, p)| p.as_slice())
        };
        // THE row-height source, shared with the hit test and
        // `cell_screen_rect` (issue #28). Built here rather than measured in
        // the loop so exactly one definition exists — see [`RowHeights`].
        let heights = RowHeights::new(self.format, view, self.col_widths);
        let row_height_of = |r: usize| -> f32 { heights.screen_height(r, &resolver, m) };

        // The frozen/split band is built FIRST and the body second, so the
        // paint loops below walk the band before the body exactly as the
        // feature describes. Both lists are viewport-sized: a band is a few
        // extra painted rows, never a second pass over the sheet.
        //
        // Each entry carries its own HEIGHT, because a wrapped row is taller
        // than its neighbours. Positions are a running sum rather than
        // `i * row_h`, so one tall row pushes the rows below it down instead
        // of being drawn over them.
        let mut row_bands: Vec<(usize, f32, f32)> =
            Vec::with_capacity(band_rows + visible_count + 1);
        let mut band_y = grid_rect.min.y;
        for i in 0..band_rows {
            let r = lead_r + i;
            if r >= total_rows {
                break;
            }
            let h = row_height_of(r);
            row_bands.push((r, band_y, h));
            band_y += h;
        }
        let body_row_start = row_bands.len();
        let mut body_y = body_rect.min.y - frac_px;
        for r in row_range.clone() {
            let h = row_height_of(r);
            row_bands.push((r, body_y, h));
            body_y += h;
            // Stop once the running sum has left the viewport. Without this a
            // screenful of 12-line rows would still walk `visible_count`
            // entries sized for single-line rows — bounded, but wasted.
            if body_y > grid_rect.max.y {
                break;
            }
        }

        let mut clicked = None;
        let mut double_clicked = None;
        let mut drag_to = None;
        let mut fill_started = false;
        let mut fill_to = None;
        let mut fill_released = false;
        let mut painted_cells = 0usize;
        let mut comment_markers = 0usize;
        let mut dropdown_button: Option<(CellRef, egui::Rect)> = None;
        let mut hovered_comment: Option<CellRef> = None;
        let mut context_click: Option<(CellRef, egui::Pos2)> = None;
        let mut painted_rows: Vec<(usize, u32)> = Vec::with_capacity(row_bands.len());
        let mut frozen_row_count = 0usize;
        // Border edges already drawn this frame, keyed by quantised geometry
        // (issue #28). See the border block in the cell loop for why sharing
        // matters. Empty and untouched on any sheet with no borders.
        let mut drawn_edges: BTreeSet<(i32, i32, i32, i32)> = BTreeSet::new();
        let mut border_segments = 0usize;
        let mut rotated_texts = 0usize;
        let mut wrapped_texts = 0usize;
        let mut sparkline_shapes = 0usize;
        let mut sparkline_blanks = 0usize;
        // Scratch buffer for ONE row's sparkline source, reused across every
        // sparklined cell in the frame. Its capacity is the widest source span
        // configured, never the sheet's row count — this is the allocation the
        // scale invariant is about, so it is hoisted here where its lifetime
        // is visibly per-frame rather than per-row.
        let mut spark_src: Vec<Option<f64>> = Vec::new();
        // A sheet with no groups at all short-circuits before any per-row or
        // per-cell work; `Option::filter` here is the only cost it pays.
        let sparklines = self.sparklines.filter(|s| !s.is_empty());

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

        // --- sheet-wide conditional formatting ---
        //
        // Built once per VISIBLE COLUMN per frame (~30 calls), never per cell
        // and never over the whole sheet. A column with no rules contributes
        // an empty plan, which costs one BTreeMap probe; a rule over a 200M-row
        // column is one entry here exactly as it is one entry in storage.
        //
        // The window-dependent rules (colour scales, data bars, top/bottom-N)
        // are evaluated against the rows ACTUALLY ON SCREEN, which is the same
        // documented approximation `TableDecor::prepare` makes. See `RuleEval`.
        let sheet_fmt = self.format;
        let mut sheet_plans: Vec<(
            usize,
            Vec<ferrix_core::PlanEntry<'a>>,
            Vec<ferrix_core::RuleEval>,
            bool,
        )> = Vec::new();
        if let Some(fmt) = sheet_fmt {
            // Underlying rows on screen, resolved once and shared by every
            // column's window scan rather than re-resolved per column.
            let mut window_rows: Vec<u32> = Vec::new();
            let mut have_rows = false;
            let mut vals: Vec<f64> = Vec::new();
            let mut scratch: Vec<f64> = Vec::new();
            for &(c, _) in &col_bands {
                let mut plan: Vec<ferrix_core::PlanEntry<'a>> = Vec::new();
                fmt.plan(c as u32, &mut plan);
                let needs_text = ferrix_core::SheetFormat::plan_needs_text(&plan);
                let mut evals: Vec<ferrix_core::RuleEval> = Vec::new();
                if ferrix_core::SheetFormat::plan_needs_window(&plan) {
                    if !have_rows {
                        for &(r, _, _) in &row_bands {
                            if let Some(sr) = resolve_row(r) {
                                if !sr.is_pad() {
                                    window_rows.push(sr.row());
                                }
                            }
                        }
                        have_rows = true;
                    }
                    vals.clear();
                    for &r in &window_rows {
                        if let Value::Number(n) = view.get(CellRef::new(r, c as u32)) {
                            vals.push(n);
                        }
                    }
                    for e in &plan {
                        // `for_rule` reorders its slice (`select_nth_unstable`),
                        // so each rule gets its own copy of the window.
                        scratch.clear();
                        scratch.extend_from_slice(&vals);
                        evals.push(ferrix_core::RuleEval::for_rule(e.rule, &mut scratch));
                    }
                }
                sheet_plans.push((c, plan, evals, needs_text));
            }
        }
        // Column -> index into `sheet_plans`, so the per-cell lookup is an
        // array index rather than a linear probe over the visible columns.
        let plan_of = |c: usize| -> Option<&(
            usize,
            Vec<ferrix_core::PlanEntry<'a>>,
            Vec<ferrix_core::RuleEval>,
            bool,
        )> { sheet_plans.iter().find(|(cc, ..)| *cc == c) };

        // --- paint ---
        //
        // The painter covers the whole grid, band included. Per-band clipping
        // is applied where it matters (below), so the frozen band cannot bleed
        // into the body and vice versa.
        let painter = ui.painter_at(grid_rect);
        painter.rect_filled(grid_rect, 0.0, th.bg);
        // Subtotal paint counters (issue #34), incremented where the rows are
        // actually drawn so they cannot claim a row that never rendered.
        let mut subtotal_rows_painted = 0usize;
        let mut subtotal_texts = 0usize;
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
        for (bi, &(r, y, row_h)) in row_bands.iter().enumerate() {
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
            // A SUBTOTAL row (issue #34) is synthetic: it holds no cell, so
            // it is drawn from the plan's already-computed group aggregates
            // and then skips the whole cell loop below. Reading `view.get()`
            // at `row` here would paint the group's last record a second
            // time under a row number that names no record at all.
            if let (ScreenRow::Subtotal { group, .. }, Some(plan)) = (resolved, self.subtotals) {
                let sp = painter.with_clip_rect(row_clip);
                sp.rect_filled(row_rect, 0.0, th.accent_soft);
                for &(c, x) in col_bands.iter() {
                    let w = width_of(c);
                    if w <= 0.0 {
                        continue;
                    }
                    let Some(cell) = plan.cell(group, c as u32) else {
                        continue;
                    };
                    let cr = Rect::from_min_size(egui::pos2(x, y), Vec2::new(w, row_h));
                    let (text, align, at) = match cell {
                        SubtotalCell::Label(s) => (
                            s,
                            Align2::LEFT_CENTER,
                            egui::pos2(cr.min.x + 4.0, cr.center().y),
                        ),
                        SubtotalCell::Number(n) => (
                            ferrix_core::format_number(n),
                            Align2::RIGHT_CENTER,
                            egui::pos2(cr.max.x - 4.0, cr.center().y),
                        ),
                        // A group with nothing numeric shows nothing, not 0 —
                        // the same honesty rule `Agg::value` enforces.
                        SubtotalCell::Blank => continue,
                    };
                    sp.text(
                        at,
                        align,
                        text,
                        FontId::proportional(11.5 * m.zoom),
                        th.text,
                    );
                    subtotal_texts += 1;
                }
                subtotal_rows_painted += 1;
                continue;
            }
            let rp = painter.with_clip_rect(row_clip);
            if is_pad {
                // Padding gets its own recessed fill and no zebra stripe, so
                // "there is no row here" is visibly different from "this row
                // exists and holds empty strings".
                rp.rect_filled(row_rect, 0.0, th.pad_row);
            } else if r % 2 == 1 {
                rp.rect_filled(row_rect, 0.0, th.row_alt);
            }

            // ONE probe per visible row, hoisted out of the column loop
            // below. A per-cell `get()` here would be ~1,500 map probes every
            // frame; `is_empty()` inside `row_comments` makes an uncommented
            // sheet cost zero. Padding rows are past the end of the sheet and
            // can hold no comment.
            let row_notes = self
                .comments
                .filter(|_| !is_pad)
                .and_then(|m| m.row_comments(row));

            // ONE probe per visible row, for the same reason: a scan of the
            // GROUP list (a handful of rectangles), hoisted out of the column
            // loop. A padding row is past the end of the sheet, so it has no
            // source values to plot and is excluded here rather than in the
            // cell loop.
            let row_has_spark = !is_pad && sparklines.is_some_and(|s| s.covers_row(row));

            for (ci, &(c, x)) in col_bands.iter().enumerate() {
                let in_lead_cols = ci < band_cols;
                let w = width_of(c);
                // A hidden column is ZERO WIDE (issue #29). Skip it before
                // anything is drawn: a zero-width `cell_rect` still
                // `intersects` the band clip — a point inside a rect counts —
                // so the check below does NOT catch this, and the cell's text
                // would be painted at the hidden column's x and spill over the
                // neighbour that took its place. The criterion is that a
                // hidden column is skipped in paint, so this is where it is
                // enforced, next to the width that defines it.
                if w <= 0.0 {
                    continue;
                }
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

                // Sheet-wide conditional formatting, resolved from the plan
                // built once for this column above. Allocates nothing per cell
                // except the display text a text rule asked for, and only when
                // one is actually configured.
                let sheet_style = match plan_of(c).filter(|(_, p, ..)| !p.is_empty()) {
                    Some((_, plan, evals, needs_text)) if !is_pad => {
                        let v = view.get(cref);
                        let text = if *needs_text {
                            view.display(cref)
                        } else {
                            String::new()
                        };
                        sheet_fmt
                            .map(|f| f.resolve(cref, &v, &text, plan, evals))
                            .filter(|s| !s.is_plain())
                    }
                    _ => None,
                };

                if let Some(s) = &sheet_style {
                    if let Some(fill) = s.fill {
                        painter.rect_filled(cell_rect, 0.0, sheet_c32(fill));
                    }
                    if let Some((frac, color)) = s.bar {
                        let inner = cell_rect.shrink2(Vec2::new(1.5, 3.0));
                        let bar = Rect::from_min_size(
                            inner.min,
                            Vec2::new(inner.width() * frac, inner.height()),
                        );
                        painter.rect_filled(bar, 1.0, sheet_c32(color));
                    }
                }

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
                // Disjoint ranges paint with the same range fill, so a
                // Ctrl+click selection LOOKS selected rather than being a
                // model-only state the user cannot see (issue #17).
                if !self.extra_selections.is_empty()
                    && self.extra_selections.iter().any(|s| s.contains(cref))
                {
                    painter.rect_filled(cell_rect, 0.0, th.range_fill);
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
                // --- sparklines (issue #36) ---
                //
                // Drawn HERE, in the cell loop, from the cell rect the value
                // would have used. There is no chart object, no series cache
                // and no per-row allocation: this row's source span is read
                // into ONE reused scratch buffer, reduced to the cell's pixel
                // width by `chart::decimate_min_max`, drawn, and forgotten.
                //
                // That is what makes the cost per VISIBLE ROW. A group over
                // 200M rows reaches this line once per row actually on screen
                // -- roughly 40 times -- exactly as a group over 40 rows does,
                // so the two sheets paint a frame in the same time.
                if row_has_spark {
                    if let Some(g) = sparklines.and_then(|s| s.group_at(cref)) {
                        // The source is read through `view.get`, the same
                        // accessor the cell text uses, so a sparkline plots
                        // the user's EDITS rather than the base file: a value
                        // the user just typed is what they expect to move the
                        // line.
                        spark_src.clear();
                        if let Some(cols) = g.source_cols(row) {
                            spark_src.reserve(g.source_len());
                            for sc in cols {
                                spark_src.push(match view.get(CellRef::new(row, sc)) {
                                    Value::Number(n) if n.is_finite() => Some(n),
                                    Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
                                    // Text, errors and empties are GAPS, not
                                    // zeros -- the same choice `numeric_column`
                                    // makes, and for the same reason: plotting
                                    // a missing measurement as 0 invents data.
                                    _ => None,
                                });
                            }
                        }
                        let inner = cell_rect.shrink2(Vec2::new(2.0 * m.zoom, 3.0 * m.zoom));
                        // An empty or entirely non-numeric source yields
                        // `None`, and the cell then draws NOTHING. Not an
                        // error marker, not a zero line: a sheet still being
                        // filled in is not a broken sheet, and a column of red
                        // flags would train the user to ignore them.
                        match ferrix_core::sparkline_shape(g.kind, &spark_src, inner.width())
                            .filter(|_| inner.width() > 1.0 && inner.height() > 1.0)
                        {
                            Some(shape) => {
                                sparkline_shapes +=
                                    paint_sparkline(&painter, inner, &shape, th, m.zoom);
                            }
                            None => sparkline_blanks += 1,
                        }
                    }
                }
                // Cell decoration (issue #28): borders, alignment, indent,
                // wrap, rotation. Resolved from the plan built once for this
                // column above, so a decorated 200M-row column costs exactly
                // what a decorated 200-row one does. A cell no scope decorates
                // gets `CellDecor::default()`, whose every field is `None`, and
                // the paint path below is then byte-for-byte what it was.
                //
                // Resolved whenever the sheet has ANY decoration, even when
                // this column's plan is empty: a per-cell override lives in
                // neither column nor range scope and so contributes no plan
                // entry, and gating on a non-empty plan silently dropped
                // every single-cell border. `resolve_decor` consults the
                // override map itself, which is why it is handed an empty
                // slice rather than skipped.
                let cd = match sheet_fmt.filter(|f| f.has_decor() && !is_pad) {
                    Some(f) => f.resolve_decor(cref, decor_plan_of(c).unwrap_or(&[])),
                    None => ferrix_core::CellDecor::default(),
                };
                // Issue #38: show formulas. The SOURCE replaces the value for
                // this cell only, pulled here in the paint loop for a cell
                // that is already being drawn — so the mode costs a viewport
                // of strings, not a sheet of them.
                let formula_src = (self.show_formulas && !is_pad)
                    .then(|| view.edit_text(cref))
                    .filter(|s| !s.is_empty());
                if formula_src.is_some() || !matches!(value, Value::Empty) {
                    let (mut text, mut color, align) = match &formula_src {
                        // Left-aligned monospace-ish source, in the accent for
                        // a real formula and ordinary ink for a literal, so
                        // "which cells are computed" is answerable at a glance
                        // — the reason to turn the mode on at all.
                        Some(src) => (
                            src.clone(),
                            if view.has_formula(cref) {
                                th.accent
                            } else {
                                th.text
                            },
                            Align2::LEFT_CENTER,
                        ),
                        None => match value {
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
                        },
                    };

                    // A table column's number format replaces the default
                    // rendering, and a conditional rule may recolour the text.
                    // Skipped entirely in show-formulas mode: a number format
                    // that rewrites `1234.5` as `$1,234.50` would overwrite
                    // the very source text the mode exists to reveal.
                    if let Some(d) = decor.as_ref().filter(|_| formula_src.is_none()) {
                        if let Some(t) = &d.text {
                            text.clone_from(t);
                        }
                        if let Some(c) = d.text_color {
                            color = c;
                        }
                    }
                    // A sheet-level rule wins over the table's colour on the
                    // cells it matches: it is the more specific instruction the
                    // user just gave, and the editor's live preview would be a
                    // lie if a table underneath could swallow it.
                    if let Some(s) = sheet_style.as_ref().filter(|_| formula_src.is_none()) {
                        if let Some(c) = s.text {
                            color = sheet_c32(c);
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

                    // --- alignment, indent and vertical placement (#28) ---
                    //
                    // An explicit horizontal alignment REPLACES the
                    // type-driven default; `HAlign::General` asks for that
                    // default back, which is why it is a variant rather than
                    // an absence.
                    let align = match cd.h_align {
                        None | Some(ferrix_core::HAlign::General) => align,
                        // Justify has no renderer here and is drawn left, the
                        // way Excel degrades it in a single-line cell. It is
                        // reported by `decor_survives_xlsx` rather than
                        // silently rewritten.
                        Some(ferrix_core::HAlign::Left) | Some(ferrix_core::HAlign::Justify) => {
                            Align2::LEFT_CENTER
                        }
                        Some(ferrix_core::HAlign::Center) => Align2::CENTER_CENTER,
                        Some(ferrix_core::HAlign::Right) => Align2::RIGHT_CENTER,
                    };
                    let pad = 6.0 * m.zoom;
                    // Indent pushes the text in from the side it is aligned
                    // to, which for a right-aligned cell is the RIGHT edge —
                    // the same thing Excel does, and the reason this is not
                    // simply added to `min.x`.
                    let ind = cd.indent_px() * m.zoom;
                    // Vertical placement only becomes observable once a row is
                    // taller than one line, which is exactly what wrapping
                    // makes happen.
                    let cy = match cd.v_align {
                        Some(ferrix_core::VAlign::Top) => cell_rect.min.y + pad,
                        Some(ferrix_core::VAlign::Bottom) => cell_rect.max.y - pad,
                        _ => cell_rect.center().y,
                    };
                    let valign = match cd.v_align {
                        Some(ferrix_core::VAlign::Top) => egui::Align::TOP,
                        Some(ferrix_core::VAlign::Bottom) => egui::Align::BOTTOM,
                        _ => egui::Align::Center,
                    };
                    let align = Align2([align.x(), valign]);
                    let anchor = match align.x() {
                        egui::Align::Max => egui::pos2(cell_rect.max.x - pad - ind, cy),
                        egui::Align::Center => egui::pos2(cell_rect.center().x, cy),
                        _ => egui::pos2(cell_rect.min.x + pad + ind, cy),
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
                    if let Some(s) = &sheet_style {
                        s.typography.apply_to(&mut ty);
                    }
                    // Resolved against the ZOOMED default, so an unstyled cell
                    // grows with the zoom and a cell with an explicit point
                    // size grows by the same factor rather than staying put.
                    let ty = ty.resolved(BASE_FONT);
                    // Shrink to fit (#28): scale the point size down until
                    // the text fits the cell's usable width, never up. Skipped
                    // for a wrapped cell — `CellDecor::shrinks` already
                    // enforces that, so the two cannot both apply.
                    let mut size = ty.size * m.zoom;
                    if cd.shrinks() {
                        let usable = (cell_rect.width() - 2.0 * pad - ind).max(1.0);
                        let est = text.chars().count() as f32
                            * ferrix_core::format::WRAP_CHAR_PX
                            * m.zoom
                            * (size / (BASE_FONT * m.zoom)).max(0.01);
                        if est > usable {
                            // Floored so a very long value stays legible
                            // rather than shrinking to an unreadable smudge.
                            size =
                                (size * usable / est).max(ferrix_core::format::MIN_FONT_PT * 0.5);
                        }
                    }

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
                    //
                    // A WRAPPED cell lays out into the cell's usable width
                    // instead, which is what makes its galley multiple lines
                    // tall — and that height is what the row was already sized
                    // for by `RowHeights`.
                    let galley = if cd.wraps() {
                        let wrap_w = cell_rect.width() - 2.0 * pad - ind;
                        // A degenerate width would ask egui to break every
                        // glyph onto its own line. Skipped explicitly HERE,
                        // next to the width that defines it, rather than
                        // relying on a downstream clip: a zero-width rect
                        // still `intersects` its clip rect, so nothing further
                        // down would catch it.
                        if wrap_w <= 0.0 {
                            painted_cells += 1;
                            continue;
                        }
                        wrapped_texts += 1;
                        clip.layout(text.clone(), font.clone(), color, wrap_w)
                    } else {
                        clip.layout_no_wrap(text.clone(), font.clone(), color)
                    };
                    let rect = align.anchor_size(anchor, galley.size());
                    // --- rotation (#28) ---
                    //
                    // Rotated text is emitted as a TextShape with an angle,
                    // which is a different shape kind from the plain galley
                    // above — so `paint_shape_count` and the shape stream
                    // both change when rotation is on, and a test can assert
                    // on that rather than on appearance.
                    let rot = cd.rotation_deg();
                    if rot != 0 {
                        // egui's angle is clockwise radians; the model's
                        // positive rotation is counter-clockwise, matching
                        // Excel's dialog. Negated here, once.
                        let mut ts = egui::epaint::TextShape::new(rect.min, galley.clone(), color);
                        ts.angle = -(rot as f32).to_radians();
                        clip.add(egui::Shape::Text(ts));
                        rotated_texts += 1;
                    } else {
                        clip.galley(rect.min, galley.clone(), color);
                    }
                    if ty.bold && rot == 0 {
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

                // --- cell borders and the diagonal (issue #28) ---
                //
                // Drawn AFTER the text so a thick border reads as a frame
                // around the value rather than a line the glyphs sit on top
                // of, and before the validation/comment flags, which are
                // corner markers that must stay on top of everything.
                //
                // SHARED EDGES ARE NOT DOUBLE-DRAWN. Two adjacent cells that
                // both ask for a border between them describe ONE line, and
                // drawing it twice is visible: the two strokes composite to a
                // heavier, darker edge than either cell asked for, and at
                // fractional zoom they land a half pixel apart and the line
                // looks doubled. So every edge is registered in `drawn_edges`
                // by its geometry, and the second cell to claim it is
                // ignored. The FIRST claimant wins, which is the left/top
                // neighbour, matching Excel's own precedence.
                if !cd.is_empty() {
                    let mut edge = |a: egui::Pos2, b: egui::Pos2, bd: ferrix_core::Border| {
                        // Quantised to 1/16px so two cells computing the same
                        // edge from opposite sides agree despite float error.
                        let key = (
                            (a.x * 16.0).round() as i32,
                            (a.y * 16.0).round() as i32,
                            (b.x * 16.0).round() as i32,
                            (b.y * 16.0).round() as i32,
                        );
                        if !drawn_edges.insert(key) {
                            return;
                        }
                        let w = bd.style.width() * m.zoom;
                        // An edge with no width draws nothing. Skipped
                        // explicitly on the dimension that defines it, next to
                        // the computation — a zero-width stroke still reaches
                        // the painter and a downstream clip check would not
                        // reject it.
                        if w <= 0.0 {
                            return;
                        }
                        // Counted once per EDGE, not once per stroke: a
                        // double border is two strokes but one edge, and a
                        // dashed one is many. The number a test asserts on has
                        // to be "how many borders were drawn", or it would
                        // change with the dash length.
                        border_segments += 1;
                        let col = bd.color.map_or(th.text, sheet_c32);
                        match bd.style {
                            ferrix_core::BorderStyle::Double => {
                                // Two thin lines with a gap, which is what
                                // "double" means — not one thick one.
                                let off = w * 0.75;
                                let n = if (a.x - b.x).abs() < f32::EPSILON {
                                    Vec2::new(off, 0.0)
                                } else {
                                    Vec2::new(0.0, off)
                                };
                                let s = Stroke::new(w * 0.5, col);
                                painter.line_segment([a - n, b - n], s);
                                painter.line_segment([a + n, b + n], s);
                            }
                            ferrix_core::BorderStyle::Dotted | ferrix_core::BorderStyle::Dashed => {
                                // Dashes are real segments rather than a
                                // stroke pattern, because egui has no dash
                                // support on a plain line and a solid line
                                // labelled "dashed" would be a lie the user
                                // can see.
                                let dash = if bd.style == ferrix_core::BorderStyle::Dotted {
                                    2.0 * m.zoom
                                } else {
                                    5.0 * m.zoom
                                };
                                let gap = dash;
                                let d = b - a;
                                let len = d.length();
                                if len <= 0.0 {
                                    return;
                                }
                                let unit = d / len;
                                let s = Stroke::new(w, col);
                                let mut t = 0.0;
                                while t < len {
                                    let e = (t + dash).min(len);
                                    painter.line_segment([a + unit * t, a + unit * e], s);
                                    t = e + gap;
                                }
                                // Explicit unit so this arm's type matches the
                                // others; the `while` is the last expression
                                // otherwise and its `()` reads as accidental.
                            }
                            _ => {
                                painter.line_segment([a, b], Stroke::new(w, col));
                            }
                        }
                    };
                    let (tl, tr2) = (cell_rect.min, egui::pos2(cell_rect.max.x, cell_rect.min.y));
                    let (bl, br2) = (egui::pos2(cell_rect.min.x, cell_rect.max.y), cell_rect.max);
                    if let Some(b) = cd.border(ferrix_core::Side::Top) {
                        edge(tl, tr2, b);
                    }
                    if let Some(b) = cd.border(ferrix_core::Side::Bottom) {
                        edge(bl, br2, b);
                    }
                    if let Some(b) = cd.border(ferrix_core::Side::Left) {
                        edge(tl, bl, b);
                    }
                    if let Some(b) = cd.border(ferrix_core::Side::Right) {
                        edge(tr2, br2, b);
                    }
                    // The diagonal is INSIDE the cell, so it can never be a
                    // shared edge and is not deduped.
                    if let Some((b, dir)) = cd.diagonal.filter(|(b, _)| b.is_visible()) {
                        let w = b.style.width() * m.zoom;
                        if w > 0.0 {
                            let col = b.color.map_or(th.text, sheet_c32);
                            let s = Stroke::new(w, col);
                            if dir.up() {
                                painter.line_segment([bl, tr2], s);
                            }
                            if dir.down() {
                                painter.line_segment([tl, br2], s);
                            }
                        }
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

                // The in-cell dropdown arrow for a validation LIST rule
                // (issue #41).
                //
                // Drawn on the SELECTION CURSOR only, the way Excel does it:
                // an arrow in every cell of a 200M-row list column would be
                // both unreadable and a lookup per painted cell. One cell per
                // frame means the cost is O(1) whatever the rule covers.
                if !is_pad
                    && self.selection.is_some_and(|s| s.cursor == cref)
                    && self
                        .validation
                        .is_some_and(|v| v.dropdown_for(cref).is_some())
                {
                    let w = (14.0 * m.zoom).min(cell_rect.width());
                    let btn = egui::Rect::from_min_max(
                        egui::pos2(cell_rect.max.x - w, cell_rect.min.y),
                        cell_rect.max,
                    );
                    painter.rect_filled(btn, 0.0, th.header_bg);
                    let c = btn.center();
                    let a = 3.0 * m.zoom;
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            egui::pos2(c.x - a, c.y - a * 0.5),
                            egui::pos2(c.x + a, c.y - a * 0.5),
                            egui::pos2(c.x, c.y + a * 0.8),
                        ],
                        th.text_dim,
                        Stroke::NONE,
                    ));
                    dropdown_button = Some((cref, btn));
                }

                // The comment marker: a small triangle in the TOP-LEFT corner.
                //
                // Deliberately the opposite corner from the validation flag,
                // and a different colour, so a cell that is both commented and
                // invalid shows both facts rather than one hiding the other.
                // Only a binary-search hit on the row's already-fetched
                // comment list; an uncommented row never reaches here.
                if let Some(notes) = row_notes {
                    if notes
                        .binary_search_by_key(&(c as u32), |(cc, _)| *cc)
                        .is_ok()
                    {
                        let tl = cell_rect.min;
                        let s = 6.0 * m.zoom;
                        painter.add(egui::Shape::convex_polygon(
                            vec![tl, egui::pos2(tl.x + s, tl.y), egui::pos2(tl.x, tl.y + s)],
                            th.comment_flag,
                            Stroke::NONE,
                        ));
                        comment_markers += 1;
                        // Hover is decided against the whole CELL, not the few
                        // pixels of the triangle: a tooltip you have to hit a
                        // 6px target to see is a tooltip nobody finds.
                        if pointer_pos.is_some_and(|p| {
                            cell_rect.contains(p) && clip_rect.contains(p) && grid_rect.contains(p)
                        }) {
                            hovered_comment = Some(cref);
                        }
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
        for (bi, &(_, y, _)) in row_bands.iter().enumerate() {
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
            // Wrapped rows (issue #28) make rows different heights, so y is
            // resolved by SEARCHING THE BAND LIST the paint loop just built
            // rather than by dividing by a uniform height. That list is the
            // painted geometry itself, so a click lands on the cell that
            // visually covers the pixel by construction — there is no second
            // height arithmetic that could drift from it.
            //
            // The uniform case keeps its division: with every row the same
            // height the two agree exactly, and division is O(1) where the
            // search is O(visible rows).
            let r = if heights.is_uniform() {
                if band_rows > 0 && pos.y < body_rect.min.y {
                    lead_r + ((pos.y - grid_rect.min.y) / row_h) as usize
                } else {
                    let dy = pos.y - body_rect.min.y + frac_px;
                    if dy < 0.0 {
                        return None;
                    }
                    first_row + (dy / row_h) as usize
                }
            } else {
                let in_band = band_rows > 0 && pos.y < body_rect.min.y;
                let mut found = None;
                for (bi, &(rr, yy, hh)) in row_bands.iter().enumerate() {
                    // A row of zero height covers no pixels; skip it
                    // explicitly here, next to the height that defines it,
                    // rather than trusting a range check — `yy..yy` contains
                    // nothing but `pos.y >= yy && pos.y < yy` is not the only
                    // spelling a future edit might reach for.
                    if hh <= 0.0 {
                        continue;
                    }
                    if (bi < body_row_start) != in_band {
                        continue;
                    }
                    if pos.y >= yy && pos.y < yy + hh {
                        found = Some(rr);
                        break;
                    }
                }
                found?
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
            let resolved = resolve_row(r)?;
            // A SUBTOTAL row is not a cell (issue #34). Selecting it would put
            // the cursor on the group's last record while the user is looking
            // at a totals row, and typing would then edit that record — data
            // changed under a row the user never aimed at. So the click lands
            // nowhere, which is what a synthetic row deserves.
            if resolved.is_subtotal() {
                return None;
            }
            Some(CellRef::new(resolved.row(), c as u32))
        };
        if primary_clicked {
            clicked = pointer_pos.and_then(hit);
        }
        if primary_double {
            double_clicked = pointer_pos.and_then(hit);
        }
        // Right-click, resolved through THE SAME hit test as a left click, so
        // the menu can never open on a different cell than a click would
        // select.
        if secondary_clicked {
            context_click = pointer_pos.and_then(|p| hit(p).map(|c| (c, p)));
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

        // --- column resize / autofit gesture (issue #29) ---
        //
        // The grab zone is a few pixels either side of a column's RIGHT edge.
        // Hidden columns are zero wide, so their edge coincides with the
        // neighbour's; `rev()` makes the last (visible) column at a given x
        // win, which is what lets the user drag the border of the column
        // AFTER a hidden one rather than silently resizing the invisible one.
        let mut resize_started = None;
        let mut resize_to = None;
        let mut resize_released = false;
        let mut col_autofit = None;
        let mut header_context = None;
        let grab = 4.0 * m.zoom;
        let edge_col = |px: f32| -> Option<usize> {
            col_bands
                .iter()
                .rev()
                .find(|&&(c, x)| {
                    let right = x + width_of(c);
                    width_of(c) > 0.0 && (px - right).abs() <= grab
                })
                .map(|&(c, _)| c)
        };
        let on_edge = pointer_pos
            .filter(|_| pointer_in_header)
            .and_then(|p| edge_col(p.x));
        if let Some(c) = on_edge {
            // A resize press must not ALSO start a header reorder drag: the
            // two gestures begin with the same button on overlapping pixels,
            // and letting both fire moved the column the user meant to widen.
            if primary_pressed {
                resize_started = pointer_pos.map(|p| (c, p.x, width_of(c)));
            }
            if primary_double {
                col_autofit = Some(c);
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        // Right-click a header: the hide/unhide menu. Resolved through the
        // same `col_at_x` a left press uses, so the menu cannot open on a
        // different column than a click would select.
        if pointer_in_header && secondary_clicked {
            header_context = pointer_pos.and_then(|p| col_at_x(p.x).map(|c| (c, p)));
        }
        // The in-flight drag is tracked from the APP's own state, not egui's
        // `is_dragging` — that flag is cleared on the release frame, so the
        // final width would be lost exactly when it is needed.
        if let Some((c, from_x, start_w)) = self.col_resizing {
            if let Some(p) = pointer_pos {
                resize_to = Some((c, (start_w + (p.x - from_x)).max(0.0)));
                // Preview line at the width the release would commit.
                hp.vline(
                    p.x,
                    outer.min.y..=outer.max.y,
                    Stroke::new(1.5_f32, th.accent),
                );
            }
            if !dragging {
                resize_released = true;
            }
        }
        if pointer_in_header {
            if primary_pressed {
                // A press on a border is a RESIZE, not a reorder.
                header_press = pointer_pos
                    .filter(|_| on_edge.is_none())
                    .and_then(|p| col_at_x(p.x));
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
        // --- outline gutter (issue #29) ---
        //
        // A strip on the LEFT of the row numbers, one indent per nesting
        // level, carrying a [-]/[+] button on each group's summary row. Width
        // is driven by how deep the outline actually goes, so a sheet with no
        // groups loses no space at all.
        let outline_depth = self.row_outline.map(|o| o.max_level()).unwrap_or(0);
        let indent = 11.0 * m.zoom;
        let gutter_w = if outline_depth == 0 {
            0.0
        } else {
            (outline_depth as f32 + 1.0) * indent
        };
        let mut outline_toggle: Option<u32> = None;
        let mut outline_buttons = 0usize;
        // --- row header selection (issue #17) ---
        //
        // The column case already existed; this is its mirror. Both are
        // resolved from the SAME band walk that paints the header, so the row
        // a press selects cannot disagree with the number drawn beside it.
        let mut row_header_press: Option<(u32, egui::Modifiers)> = None;
        let mut row_header_hitboxes: Vec<(u32, egui::Pos2)> = Vec::new();
        let pointer_in_row_header = pointer_pos.is_some_and(|p| row_header.contains(p));
        let modifiers = ui.input(|i| i.modifiers);
        // Row headers carry the ORIGINAL row number even under a filter. A
        // filtered view that renumbered its rows 1..N would be actively
        // misleading: the whole point of finding row 4,912,733 is knowing it
        // is row 4,912,733.
        //
        // Walks the SAME band list as the cells — frozen band first, then the
        // body — so the number beside a frozen row is that row's number no
        // matter where the body has scrolled to.
        for (bi, &(r, y, row_h)) in row_bands.iter().enumerate() {
            let Some(resolved) = resolve_row(r) else {
                continue;
            };
            let row = resolved.row();
            // Recorded from the SAME walk that paints the row number, so the
            // reported "what is on screen" cannot disagree with what is.
            // A SUBTOTAL row is skipped (issue #34): it is not a row of the
            // sheet, so recording it here would make `painted_rows` — which
            // tests and the cell editor read as "these records are on screen"
            // — claim the group's last record twice.
            if resolved.is_subtotal() {
                if bi < body_row_start {
                    frozen_row_count += 1;
                }
                // The gutter shows the group marker instead of a row number,
                // because there IS no row number: naming the group's last
                // record here would point the user at a record that is also
                // drawn on its own line just above.
                let rect = Rect::from_min_size(
                    egui::pos2(outer.min.x, y),
                    Vec2::new(m.row_header_w, row_h),
                );
                rhp.rect_filled(rect, 0.0, th.accent_soft);
                rhp.text(
                    egui::pos2(rect.max.x - 8.0 * m.zoom, rect.center().y),
                    Align2::RIGHT_CENTER,
                    "\u{2211}",
                    FontId::proportional(11.5 * m.zoom),
                    th.accent,
                );
                continue;
            }
            painted_rows.push((r, row));
            if bi < body_row_start {
                frozen_row_count += 1;
            }
            // Row numbers name the DATA row, so a filtered view shows the
            // original 1, 5, 9, ... rather than renumbering to 1, 2, 3 — the
            // user must always be able to tell which rows are hidden.
            let rect =
                Rect::from_min_size(egui::pos2(outer.min.x, y), Vec2::new(m.row_header_w, row_h));
            // The number is pushed right of the outline gutter so a group
            // spine never draws through it.
            let num_rect =
                Rect::from_min_max(egui::pos2(rect.min.x + gutter_w, rect.min.y), rect.max);
            let selected = self.selection.is_some_and(|s| {
                let (a, b) = s.row_range();
                row >= a && row <= b
            }) || self.extra_selections.iter().any(|s| {
                let (a, b) = s.row_range();
                row >= a && row <= b
            });
            if resolved.is_pad() {
                rhp.rect_filled(rect, 0.0, th.pad_row);
            }
            if selected {
                rhp.rect_filled(rect, 0.0, th.accent_soft);
            }
            // Outline gutter for this row: the nesting spine, plus the
            // collapse/expand button on a group's first row. Only real rows
            // carry groups — padding is past the end of every range.
            let mut on_outline_button = false;
            if let (Some(outline), false) = (self.row_outline, resolved.is_pad()) {
                let level = outline.level_at(row);
                if level > 0 {
                    let x = rect.min.x + (level as f32 - 0.5) * indent;
                    rhp.vline(
                        x,
                        rect.min.y..=rect.max.y,
                        Stroke::new(1.0_f32, th.grid_line),
                    );
                }
                if let Some(g) = outline.group_starting_at(row) {
                    let bx = rect.min.x + (g.level as f32 - 0.5) * indent;
                    let btn = Rect::from_center_size(
                        egui::pos2(bx, rect.center().y),
                        Vec2::splat(9.0 * m.zoom),
                    );
                    rhp.rect_filled(btn, 1.0, th.panel);
                    rhp.rect_stroke(btn, 1.0, Stroke::new(1.0_f32, th.text_dim));
                    // Minus when open, plus when collapsed — the same glyph
                    // convention Excel uses.
                    rhp.hline(
                        btn.min.x + 2.0..=btn.max.x - 2.0,
                        btn.center().y,
                        Stroke::new(1.2_f32, th.text),
                    );
                    if g.collapsed {
                        rhp.vline(
                            btn.center().x,
                            btn.min.y + 2.0..=btn.max.y - 2.0,
                            Stroke::new(1.2_f32, th.text),
                        );
                    }
                    outline_buttons += 1;
                    // Hit-tested here, from the rect that was just painted, so
                    // the button cannot drift from where it is drawn.
                    if primary_clicked && pointer_pos.is_some_and(|p| btn.expand(2.0).contains(p)) {
                        outline_toggle = Some(row);
                        // An outline toggle is NOT also a row selection: the
                        // two controls overlap, and letting both fire would
                        // reselect the sheet every time a group was collapsed.
                        on_outline_button = true;
                    }
                }
            }
            // Recorded from the SAME rect the number was painted into, so a
            // caller aiming at the reported centre hits the row it names.
            // Padding rows are excluded: they are not rows of the file yet, and
            // selecting one would extend the sheet to create it.
            if !resolved.is_pad() {
                row_header_hitboxes.push((row, rect.center()));
                if pointer_in_row_header
                    && primary_pressed
                    && !on_outline_button
                    && pointer_pos.is_some_and(|p| rect.contains(p))
                {
                    row_header_press = Some((row, modifiers));
                }
            }
            // A padding row still shows its would-be number, so the user can
            // see where they are, but dimmed — it is not a row of the file
            // yet. Typing into it is what makes it one.
            rhp.text(
                egui::pos2(num_rect.max.x - 8.0 * m.zoom, num_rect.center().y),
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
            dropdown_button,
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
            hovered_comment,
            comment_markers,
            context_click,
            resize_started,
            resize_to,
            resize_released,
            col_autofit,
            header_context,
            row_header_press,
            row_header_hitboxes,
            outline_toggle,
            outline_buttons,
            subtotal_rows: subtotal_rows_painted,
            subtotal_texts,
            border_segments,
            rotated_texts,
            wrapped_texts,
            sparkline_shapes,
            sparkline_blanks,
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
