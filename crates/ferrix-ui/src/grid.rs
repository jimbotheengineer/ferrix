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
use ferrix_core::{column_name, CellRef, Selection, Value};

use crate::sheet_view::SheetView;
use crate::theme::Theme;

pub const FILL_HANDLE: f32 = 7.0;
pub const ROW_HEIGHT: f32 = 22.0;
pub const DEFAULT_COL_WIDTH: f32 = 108.0;
pub const ROW_HEADER_WIDTH: f32 = 88.0;
pub const HEADER_HEIGHT: f32 = 26.0;
const SCROLLBAR_W: f32 = 12.0;

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
}

impl<'a> Grid<'a> {
    /// Screen rect of a cell, or None when it is scrolled out of view. The
    /// editor uses this to place its TextEdit exactly over the cell.
    pub fn cell_screen_rect(
        cell: CellRef,
        outer: Rect,
        scroll: &ScrollState,
        col_widths: &[f32],
    ) -> Option<Rect> {
        let body_origin = outer.min + Vec2::new(ROW_HEADER_WIDTH, HEADER_HEIGHT);
        let rel_row = cell.row as f64 - scroll.row_offset;
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
        let total_rows = view.row_count().max(1);
        let total_cols = view.col_count().max(1);

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
        let vis_lo = self
            .matches
            .partition_point(|m| (m.row as usize) < first_row);
        let vis_hi = self
            .matches
            .partition_point(|m| (m.row as usize) < last_row);
        let visible_matches = &self.matches[vis_lo..vis_hi];
        let is_match = |cell: CellRef| -> bool {
            visible_matches
                .binary_search_by(|m| (m.row, m.col).cmp(&(cell.row, cell.col)))
                .is_ok()
        };

        // --- paint body ---
        let painter = ui.painter_at(body_rect);
        painter.rect_filled(body_rect, 0.0, Theme::BG);

        for r in row_range.clone() {
            let y = body_origin.y + (r - first_row) as f32 * ROW_HEIGHT - frac_px;
            let row_rect = Rect::from_min_size(
                egui::pos2(body_rect.min.x, y),
                Vec2::new(body_rect.width(), ROW_HEIGHT),
            );
            if r % 2 == 1 {
                painter.rect_filled(row_rect, 0.0, Theme::ROW_ALT);
            }

            for c in col_range.clone() {
                let x = body_origin.x + col_x[c] - self.scroll.col_px;
                let w = width_of(c);
                let cell_rect = Rect::from_min_size(egui::pos2(x, y), Vec2::new(w, ROW_HEIGHT));
                let cref = CellRef::new(r as u32, c as u32);

                // Search highlight sits under the selection so both remain
                // visible when the cursor is parked on a match.
                if !visible_matches.is_empty() && is_match(cref) {
                    if self.current_match == Some(cref) {
                        painter.rect_filled(cell_rect, 0.0, Theme::MATCH_CURRENT);
                        painter.rect_stroke(
                            cell_rect,
                            0.0,
                            Stroke::new(1.5_f32, Theme::MATCH_EDGE),
                        );
                    } else {
                        painter.rect_filled(cell_rect, 0.0, Theme::MATCH_BG);
                    }
                }

                // Selection painting. A range gets a translucent fill; the
                // cursor cell keeps the strong border so the user can always
                // see where typing will land.
                if let Some(sel) = self.selection {
                    if sel.cursor == cref {
                        painter.rect_filled(cell_rect, 0.0, Theme::ACCENT_SOFT);
                        painter.rect_stroke(cell_rect, 0.0, Stroke::new(1.5_f32, Theme::ACCENT));
                    } else if !sel.is_single() && sel.contains(cref) {
                        painter.rect_filled(cell_rect, 0.0, Theme::RANGE_FILL);
                    }
                }

                // The cell under edit is drawn by the caller's TextEdit.
                if self.editing == Some(cref) {
                    painted_cells += 1;
                    continue;
                }

                let value = view.get(cref);
                if !matches!(value, Value::Empty) {
                    let (text, color, align) = match value {
                        Value::Number(n) => (
                            ferrix_core::format_number(n),
                            Theme::NUMBER,
                            Align2::RIGHT_CENTER,
                        ),
                        Value::Bool(b) => (
                            if b { "TRUE" } else { "FALSE" }.to_string(),
                            Theme::TEXT_DIM,
                            Align2::CENTER_CENTER,
                        ),
                        Value::Text(id) => (
                            view.resolve(id).to_string(),
                            Theme::TEXT,
                            Align2::LEFT_CENTER,
                        ),
                        Value::Error(e) => (e.to_string(), Theme::ERROR, Align2::RIGHT_CENTER),
                        Value::Empty => unreachable!(),
                    };

                    // A formula cell gets a subtle marker so it is
                    // distinguishable from a typed-in literal.
                    if view.has_formula(cref) {
                        painter.circle_filled(
                            egui::pos2(cell_rect.min.x + 3.5, cell_rect.min.y + 4.0),
                            1.6,
                            Theme::ACCENT,
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
                painted_cells += 1;
            }
        }

        // --- grid lines ---
        let line = Stroke::new(1.0_f32, Theme::GRID_LINE);
        for r in row_range.clone() {
            let y = body_origin.y + (r - first_row) as f32 * ROW_HEIGHT - frac_px;
            painter.hline(body_rect.min.x..=body_rect.max.x, y, line);
        }
        for c in col_range.start..=col_range.end.min(total_cols) {
            let x = body_origin.x + col_x[c.min(total_cols)] - self.scroll.col_px;
            painter.vline(x, body_rect.min.y..=body_rect.max.y, line);
        }

        // --- hit testing ---
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
            Some(CellRef::new(r as u32, c as u32))
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
                    Self::cell_screen_rect(br, outer, self.scroll, self.col_widths).map(|r| {
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
                painter.rect_filled(hr, 1.0, Theme::ACCENT);
                painter.rect_stroke(hr, 1.0, Stroke::new(1.0_f32, Theme::BG));
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
        vpainter.rect_filled(vbar, 0.0, Theme::PANEL);
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
            if vbar_active {
                Theme::ACCENT
            } else {
                Theme::GRID_LINE
            },
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
        hpainter.rect_filled(hbar, 0.0, Theme::PANEL);
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
                    Theme::ACCENT
                } else {
                    Theme::GRID_LINE
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
        hp.rect_filled(col_header, 0.0, Theme::HEADER_BG);
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
                Theme::TEXT_DIM,
            );
            chp.vline(r.max.x, outer.min.y..=outer.min.y + HEADER_HEIGHT, line);
        }

        let row_header = Rect::from_min_size(
            egui::pos2(outer.min.x, body_origin.y),
            Vec2::new(ROW_HEADER_WIDTH, body_rect.height()),
        );
        hp.rect_filled(row_header, 0.0, Theme::HEADER_BG);
        let rhp = hp.with_clip_rect(row_header);
        for r in row_range.clone() {
            let y = body_origin.y + (r - first_row) as f32 * ROW_HEIGHT - frac_px;
            let rect = Rect::from_min_size(
                egui::pos2(outer.min.x, y),
                Vec2::new(ROW_HEADER_WIDTH, ROW_HEIGHT),
            );
            let selected = self.selection.is_some_and(|s| {
                let (a, b) = s.row_range();
                r >= a as usize && r <= b as usize
            });
            if selected {
                rhp.rect_filled(rect, 0.0, Theme::ACCENT_SOFT);
            }
            rhp.text(
                egui::pos2(rect.max.x - 8.0, rect.center().y),
                Align2::RIGHT_CENTER,
                (r + 1).to_string(),
                FontId::proportional(11.5),
                if selected {
                    Theme::ACCENT
                } else {
                    Theme::TEXT_DIM
                },
            );
        }

        hp.rect_filled(
            Rect::from_min_size(outer.min, Vec2::new(ROW_HEADER_WIDTH, HEADER_HEIGHT)),
            0.0,
            Theme::HEADER_BG,
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
}
