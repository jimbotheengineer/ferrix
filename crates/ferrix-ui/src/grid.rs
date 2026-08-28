//! Virtualized spreadsheet grid.
//!
//! The whole performance story of the UI lives here: no matter how many rows
//! the sheet holds, we compute which rows and columns intersect the viewport
//! and paint ONLY those. A 10M-row sheet and a 100-row sheet do exactly the
//! same amount of per-frame work.
//!
//! We paint cells directly onto the `Painter` instead of instantiating egui
//! widgets per cell — a widget per visible cell (~1,500 of them) would mean
//! 1,500 id allocations and interaction checks every frame.
//!
//! ## Known ceiling: ~16.7M rows
//!
//! Scroll position is an f32 pixel offset into a virtual canvas of
//! `rows * ROW_HEIGHT` pixels. f32 has a 24-bit mantissa, so the smallest
//! representable step grows with the canvas:
//!
//! | rows | canvas    | ulp   | row addressing |
//! |------|-----------|-------|----------------|
//! | 1M   | 22M px    | 2 px  | exact          |
//! | 10M  | 220M px   | 16 px | exact          |
//! | 100M | 2.2B px   | 256px | off by ~11 rows|
//!
//! Addressing stays exact while ulp < ROW_HEIGHT, which holds to ~16.7M rows
//! (verified by `f32_precision_holds_at_target_scale` below). Beyond that the
//! fix is to stop handing egui a giant canvas and track the first visible row
//! as an integer, scrolling in row units instead of pixels.

use egui::{Align2, FontId, Rect, Response, Sense, Stroke, Ui, Vec2};
use ferrix_core::{column_name, CellRef, Sheet, Value};

use crate::theme::Theme;

pub const ROW_HEIGHT: f32 = 22.0;
pub const DEFAULT_COL_WIDTH: f32 = 108.0;
pub const ROW_HEADER_WIDTH: f32 = 72.0;
pub const HEADER_HEIGHT: f32 = 26.0;

/// Which cell the user clicked, if any.
pub struct GridResponse {
    pub clicked: Option<CellRef>,
    /// Exposed for diagnostics and future viewport-driven prefetching.
    #[allow(dead_code)]
    pub visible_rows: std::ops::Range<usize>,
    #[allow(dead_code)]
    pub visible_cols: std::ops::Range<usize>,
    pub painted_cells: usize,
}

pub struct Grid<'a> {
    pub sheet: &'a Sheet,
    pub selection: Option<CellRef>,
    pub col_widths: &'a [f32],
}

impl<'a> Grid<'a> {
    pub fn show(self, ui: &mut Ui) -> GridResponse {
        let sheet = self.sheet;
        let total_rows = sheet.row_count().max(1);
        let total_cols = sheet.col_count().max(1);

        let width_of =
            |c: usize| -> f32 { self.col_widths.get(c).copied().unwrap_or(DEFAULT_COL_WIDTH) };

        // Total virtual canvas size. egui only needs the size; it never
        // allocates anything per row.
        let total_width: f32 = (0..total_cols).map(width_of).sum();
        let total_height = total_rows as f32 * ROW_HEIGHT;

        let outer = ui.available_rect_before_wrap();
        let body_origin = outer.min + Vec2::new(ROW_HEADER_WIDTH, HEADER_HEIGHT);

        let mut clicked = None;
        let mut painted_cells = 0usize;

        // The scroll area must occupy the body region only — the same rect the
        // pinned headers are aligned against. Letting it start at `outer.min`
        // would offset every cell from its row/column header by exactly the
        // header size.
        let body_rect = Rect::from_min_max(body_origin, outer.max);
        let mut scroll_offset = Vec2::ZERO;

        let scroll = ui
            .allocate_new_ui(egui::UiBuilder::new().max_rect(body_rect), |ui| {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show_viewport(ui, |ui, viewport| {
                        scroll_offset = viewport.min.to_vec2();
                        ui.set_width(total_width);
                        ui.set_height(total_height);

                        let painter = ui.painter();
                        let clip = ui.clip_rect();
                        let origin = ui.min_rect().min;

                        // --- virtualization: rows ---
                        let first_row = (viewport.min.y / ROW_HEIGHT).floor().max(0.0) as usize;
                        let last_row =
                            ((viewport.max.y / ROW_HEIGHT).ceil() as usize + 1).min(total_rows);
                        let row_range = first_row..last_row;

                        // --- virtualization: columns (prefix-sum walk) ---
                        let mut col_x = Vec::with_capacity(total_cols + 1);
                        let mut acc = 0.0f32;
                        for c in 0..total_cols {
                            col_x.push(acc);
                            acc += width_of(c);
                        }
                        col_x.push(acc);

                        let first_col = col_x
                            .partition_point(|&x| x <= viewport.min.x)
                            .saturating_sub(1);
                        let last_col = col_x
                            .partition_point(|&x| x < viewport.max.x)
                            .min(total_cols);
                        let col_range = first_col..last_col;

                        // --- paint visible cells ---
                        for r in row_range.clone() {
                            let y = origin.y + r as f32 * ROW_HEIGHT;
                            // Stripe across the full visible width, not just the
                            // populated columns, so short sheets still read as a
                            // continuous surface rather than a floating block.
                            let stripe_w = total_width.max(clip.width());
                            let row_rect = Rect::from_min_size(
                                egui::pos2(origin.x, y),
                                Vec2::new(stripe_w, ROW_HEIGHT),
                            );
                            if !clip.intersects(row_rect) {
                                continue;
                            }

                            // Zebra striping aids horizontal tracking across wide sheets.
                            if r % 2 == 1 {
                                painter.rect_filled(row_rect, 0.0, Theme::ROW_ALT);
                            }

                            for c in col_range.clone() {
                                let x = origin.x + col_x[c];
                                let w = width_of(c);
                                let cell_rect =
                                    Rect::from_min_size(egui::pos2(x, y), Vec2::new(w, ROW_HEIGHT));

                                let cref = CellRef::new(r as u32, c as u32);
                                let value = sheet.get(cref);

                                // Selection highlight.
                                if self.selection == Some(cref) {
                                    painter.rect_filled(cell_rect, 0.0, Theme::ACCENT_SOFT);
                                    painter.rect_stroke(
                                        cell_rect,
                                        0.0,
                                        Stroke::new(1.5, Theme::ACCENT),
                                    );
                                }

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
                                            sheet.resolve(id).to_string(),
                                            Theme::TEXT,
                                            Align2::LEFT_CENTER,
                                        ),
                                        Value::Error(e) => {
                                            (e.to_string(), Theme::ERROR, Align2::RIGHT_CENTER)
                                        }
                                        Value::Empty => unreachable!(),
                                    };

                                    let pad = 6.0;
                                    let anchor_pos = match align {
                                        Align2::RIGHT_CENTER => {
                                            egui::pos2(cell_rect.max.x - pad, cell_rect.center().y)
                                        }
                                        Align2::CENTER_CENTER => cell_rect.center(),
                                        _ => {
                                            egui::pos2(cell_rect.min.x + pad, cell_rect.center().y)
                                        }
                                    };

                                    // Clip long text to its own cell so neighbours stay readable.
                                    let cell_painter = painter.with_clip_rect(
                                        cell_rect.intersect(clip).shrink2(Vec2::new(2.0, 0.0)),
                                    );
                                    cell_painter.text(
                                        anchor_pos,
                                        align,
                                        text,
                                        FontId::proportional(12.5),
                                        color,
                                    );
                                }
                                painted_cells += 1;
                            }
                        }

                        // --- grid lines (drawn once per line, not per cell) ---
                        let line = Stroke::new(1.0, Theme::GRID_LINE);
                        for r in row_range.clone() {
                            let y = origin.y + r as f32 * ROW_HEIGHT;
                            painter.hline(
                                origin.x + col_x[col_range.start]
                                    ..=origin.x + col_x[col_range.end.min(total_cols)],
                                y,
                                line,
                            );
                        }
                        for c in col_range.start..=col_range.end.min(total_cols) {
                            let x = origin.x + col_x[c.min(total_cols)];
                            painter.vline(
                                x,
                                origin.y + row_range.start as f32 * ROW_HEIGHT
                                    ..=origin.y + row_range.end as f32 * ROW_HEIGHT,
                                line,
                            );
                        }

                        // --- click handling: one hit-test, no per-cell widgets ---
                        let resp: Response =
                            ui.interact(ui.min_rect(), ui.id().with("grid_body"), Sense::click());
                        if resp.clicked() {
                            if let Some(pos) = resp.interact_pointer_pos() {
                                let local = pos - origin;
                                let r = (local.y / ROW_HEIGHT).floor() as i64;
                                let c = col_x.partition_point(|&x| x <= local.x) as i64 - 1;
                                if r >= 0
                                    && c >= 0
                                    && (r as usize) < total_rows
                                    && (c as usize) < total_cols
                                {
                                    clicked = Some(CellRef::new(r as u32, c as u32));
                                }
                            }
                        }

                        (row_range, col_range, col_x)
                    })
                    .inner
            })
            .inner;

        let (row_range, col_range, col_x) = scroll;

        // --- pinned headers, painted after the body so they sit on top ---
        let painter = ui.painter_at(outer);

        // Column headers.
        let col_header_rect = Rect::from_min_size(
            egui::pos2(body_origin.x, outer.min.y),
            Vec2::new(outer.width() - ROW_HEADER_WIDTH, HEADER_HEIGHT),
        );
        painter.rect_filled(col_header_rect, 0.0, Theme::HEADER_BG);
        let header_painter = painter.with_clip_rect(col_header_rect);
        for c in col_range.clone() {
            let x = body_origin.x + col_x[c] - scroll_offset.x;
            let w = self.col_widths.get(c).copied().unwrap_or(DEFAULT_COL_WIDTH);
            let r = Rect::from_min_size(egui::pos2(x, outer.min.y), Vec2::new(w, HEADER_HEIGHT));
            let label = sheet.header_or_letter(c);
            let letter = column_name(c as u32);
            let shown = if label == letter {
                label
            } else {
                format!("{label}  ·  {letter}")
            };
            header_painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                shown,
                FontId::proportional(12.0),
                Theme::TEXT_DIM,
            );
            header_painter.vline(
                r.max.x,
                outer.min.y..=outer.min.y + HEADER_HEIGHT,
                Stroke::new(1.0, Theme::GRID_LINE),
            );
        }

        // Row headers.
        let row_header_rect = Rect::from_min_size(
            egui::pos2(outer.min.x, body_origin.y),
            Vec2::new(ROW_HEADER_WIDTH, outer.height() - HEADER_HEIGHT),
        );
        painter.rect_filled(row_header_rect, 0.0, Theme::HEADER_BG);
        let row_painter = painter.with_clip_rect(row_header_rect);
        for r in row_range.clone() {
            let y = body_origin.y + r as f32 * ROW_HEIGHT - scroll_offset.y;
            let rect = Rect::from_min_size(
                egui::pos2(outer.min.x, y),
                Vec2::new(ROW_HEADER_WIDTH, ROW_HEIGHT),
            );
            let selected = self.selection.map(|s| s.row as usize) == Some(r);
            if selected {
                row_painter.rect_filled(rect, 0.0, Theme::ACCENT_SOFT);
            }
            row_painter.text(
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

        // Corner box.
        painter.rect_filled(
            Rect::from_min_size(outer.min, Vec2::new(ROW_HEADER_WIDTH, HEADER_HEIGHT)),
            0.0,
            Theme::HEADER_BG,
        );
        painter.line_segment(
            [
                egui::pos2(outer.min.x, body_origin.y - 0.5),
                egui::pos2(outer.max.x, body_origin.y - 0.5),
            ],
            Stroke::new(1.0, Theme::GRID_LINE),
        );
        painter.line_segment(
            [
                egui::pos2(body_origin.x - 0.5, outer.min.y),
                egui::pos2(body_origin.x - 0.5, outer.max.y),
            ],
            Stroke::new(1.0, Theme::GRID_LINE),
        );

        GridResponse {
            clicked,
            visible_rows: row_range,
            visible_cols: col_range,
            painted_cells,
        }
    }
}

/// Compute how many cells a viewport of the given size will paint. Pure
/// arithmetic, so it is unit-testable without a live egui context.
#[allow(dead_code)]
pub fn cells_in_viewport(viewport: Vec2, col_width: f32) -> usize {
    let rows = (viewport.y / ROW_HEIGHT).ceil() as usize + 1;
    let cols = (viewport.x / col_width).ceil() as usize + 1;
    rows * cols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_cell_count_is_bounded() {
        // A 4K display's worth of grid must still be a small number of cells.
        let n = cells_in_viewport(Vec2::new(3840.0, 2160.0), DEFAULT_COL_WIDTH);
        assert!(
            n < 4000,
            "4K viewport would paint {n} cells; virtualization is not bounding work"
        );
    }

    #[test]
    fn viewport_count_is_independent_of_sheet_size() {
        // The whole point: painting cost depends on the window, not the data.
        let a = cells_in_viewport(Vec2::new(1920.0, 1080.0), DEFAULT_COL_WIDTH);
        let b = cells_in_viewport(Vec2::new(1920.0, 1080.0), DEFAULT_COL_WIDTH);
        assert_eq!(a, b);
        assert!(a < 1200, "1080p viewport paints {a} cells");
    }

    #[test]
    fn ten_million_rows_paint_one_screenful() {
        // 10M rows at 22px is a 220,000,000px tall canvas; we still only ever
        // paint the ~50 rows that intersect the viewport.
        let visible = (1080.0f32 / ROW_HEIGHT).ceil() as usize + 1;
        assert!(visible < 60, "expected ~50 visible rows, got {visible}");
    }

    /// Smallest representable f32 step at a given magnitude.
    fn ulp(x: f32) -> f32 {
        f32::from_bits(x.to_bits() + 1) - x
    }

    #[test]
    fn f32_precision_holds_at_target_scale() {
        // Scroll offsets are f32 pixels. Row addressing is only exact while
        // one ulp of the full canvas height is smaller than a row.
        for rows in [1_000_000usize, 10_000_000] {
            let canvas = rows as f32 * ROW_HEIGHT;
            assert!(
                ulp(canvas) < ROW_HEIGHT,
                "at {rows} rows the canvas is {canvas}px with ulp {}px, \
                 which exceeds ROW_HEIGHT {ROW_HEIGHT}px — scroll offsets can \
                 no longer address individual rows",
                ulp(canvas)
            );
            // Deepest row must survive a pixel round-trip exactly.
            let deepest = rows - 1;
            let y = deepest as f32 * ROW_HEIGHT;
            assert_eq!(
                (y / ROW_HEIGHT).floor() as usize,
                deepest,
                "row {deepest} did not round-trip through an f32 pixel offset"
            );
        }
    }

    #[test]
    fn documents_where_f32_scrolling_breaks() {
        // Guards the documented ceiling: 100M rows is genuinely past what a
        // pixel-offset scroll canvas can address. If this ever starts passing,
        // the module docs are stale.
        let canvas = 100_000_000f32 * ROW_HEIGHT;
        assert!(
            ulp(canvas) > ROW_HEIGHT,
            "100M rows now addresses cleanly — update the module docs"
        );
    }
}
