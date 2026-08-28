//! Application state and top-level layout.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use egui::{Align, Key, Layout, RichText};
use ferrix_core::{CellRef, Sheet, Value};
use ferrix_formula::{eval_view, parse};
use ferrix_io::{load_csv, CsvOptions, LoadStats};

use crate::grid::{Grid, ScrollState, DEFAULT_COL_WIDTH};
use crate::theme::Theme;
use crate::workbook::Workbook;

type LoadResult = Result<(Sheet, LoadStats), String>;

/// Where keyboard input should go.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Grid,
    /// Editing in-cell; the buffer holds what has been typed so far.
    Cell,
    FormulaBar,
}

pub struct FerrixApp {
    wb: Workbook,
    stats: Option<LoadStats>,
    col_widths: Vec<f32>,
    selection: CellRef,
    scroll: ScrollState,

    focus: Focus,
    editing: Option<CellRef>,
    edit_buffer: String,
    /// True on the frame an edit begins, so we can grab keyboard focus once.
    just_started_edit: bool,

    formula_input: String,
    formula_result: Option<String>,

    status: String,
    loading: bool,
    load_rx: Option<Receiver<LoadResult>>,
    frame_ms: f32,
    last_painted: usize,
    /// Height of the grid body last frame, used for page-up/down sizing.
    last_viewport_h: f32,
}

impl FerrixApp {
    pub fn new(initial: Option<PathBuf>) -> Self {
        let mut app = Self {
            wb: Workbook::new(Sheet::new("Sheet1")),
            stats: None,
            col_widths: Vec::new(),
            selection: CellRef::new(0, 0),
            scroll: ScrollState::default(),
            focus: Focus::Grid,
            editing: None,
            edit_buffer: String::new(),
            just_started_edit: false,
            formula_input: String::new(),
            formula_result: None,
            status: "Ready — open a CSV to begin".into(),
            loading: false,
            load_rx: None,
            frame_ms: 0.0,
            last_painted: 0,
            last_viewport_h: 600.0,
        };
        if let Some(p) = initial {
            app.start_load(p);
        }
        app
    }

    fn start_load(&mut self, path: PathBuf) {
        let (tx, rx) = channel();
        self.loading = true;
        self.status = format!("Loading {}…", path.display());
        std::thread::spawn(move || {
            let result = load_csv(&path, CsvOptions::default()).map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.load_rx = Some(rx);
    }

    fn poll_load(&mut self) {
        let Some(rx) = &self.load_rx else { return };
        match rx.try_recv() {
            Ok(Ok((sheet, stats))) => {
                self.col_widths = compute_col_widths(&sheet);
                self.status = format!(
                    "Loaded {} rows × {} cols in {} ms ({:.0} MB/s) · {:.2} GB resident",
                    fmt_int(stats.rows),
                    stats.cols,
                    stats.parse_millis,
                    stats.throughput_mbps(),
                    sheet.heap_bytes() as f64 / 1e9,
                );
                self.wb = Workbook::new(sheet);
                self.stats = Some(stats);
                self.selection = CellRef::new(0, 0);
                self.scroll = ScrollState::default();
                self.loading = false;
                self.load_rx = None;
            }
            Ok(Err(e)) => {
                self.status = format!("Load failed: {e}");
                self.loading = false;
                self.load_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.status = "Load thread died".into();
                self.loading = false;
                self.load_rx = None;
            }
        }
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv", "tsv", "txt"])
            .pick_file()
        {
            self.start_load(path);
        }
    }

    fn begin_edit(&mut self, cell: CellRef, seed: Option<String>) {
        self.editing = Some(cell);
        self.edit_buffer = seed.unwrap_or_else(|| self.wb.view().edit_text(cell));
        self.focus = Focus::Cell;
        self.just_started_edit = true;
    }

    fn commit_edit(&mut self) {
        let Some(cell) = self.editing.take() else {
            return;
        };
        let raw = std::mem::take(&mut self.edit_buffer);
        let report = self.wb.commit_edit(cell, &raw);
        self.focus = Focus::Grid;

        self.status = if let Some(err) = &report.parse_error {
            format!("{}: {err}", cell.to_a1())
        } else if report.circular {
            format!("{}: circular reference", cell.to_a1())
        } else if report.recalculated > 0 {
            format!(
                "{} updated · {} dependent{} recalculated in {} µs",
                cell.to_a1(),
                report.recalculated,
                if report.recalculated == 1 { "" } else { "s" },
                report.micros
            )
        } else {
            format!("{} updated ({} µs)", cell.to_a1(), report.micros)
        };
        self.sync_formula_bar();
    }

    fn cancel_edit(&mut self) {
        self.editing = None;
        self.edit_buffer.clear();
        self.focus = Focus::Grid;
    }

    /// Move the selection, committing any in-progress edit first.
    fn move_selection(&mut self, drow: i64, dcol: i64) {
        if self.editing.is_some() {
            self.commit_edit();
        }
        let view = self.wb.view();
        let max_row = view.row_count().saturating_sub(1) as i64;
        let max_col = view.col_count().saturating_sub(1) as i64;
        let r = (self.selection.row as i64 + drow).clamp(0, max_row.max(0));
        let c = (self.selection.col as i64 + dcol).clamp(0, max_col.max(0));
        self.selection = CellRef::new(r as u32, c as u32);
        self.scroll_to_selection();
        self.sync_formula_bar();
    }

    /// Keep the selected cell on screen after keyboard navigation.
    fn scroll_to_selection(&mut self) {
        let visible = (self.viewport_rows() as f64 - 1.0).max(1.0);
        let row = self.selection.row as f64;
        if row < self.scroll.row_offset {
            self.scroll.row_offset = row;
        } else if row >= self.scroll.row_offset + visible {
            self.scroll.row_offset = row - visible + 1.0;
        }
    }

    fn viewport_rows(&self) -> usize {
        // Recomputed each frame from the real rect; this is a safe default
        // used only before the first paint.
        ((self.last_viewport_h.max(200.0)) / crate::grid::ROW_HEIGHT) as usize
    }

    fn sync_formula_bar(&mut self) {
        self.formula_input = self.wb.view().edit_text(self.selection);
        self.recompute_formula();
    }

    fn recompute_formula(&mut self) {
        let text = self.formula_input.trim().to_string();
        if !text.starts_with('=') {
            self.formula_result = None;
            return;
        }
        self.formula_result = Some(match parse(&text) {
            Ok(expr) => {
                let t = std::time::Instant::now();
                let view = self.wb.view();
                let v = eval_view(&expr, &view);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let shown = match v {
                    Value::Number(n) => ferrix_core::format_number(n),
                    Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
                    Value::Text(id) => view.resolve(id).to_string(),
                    Value::Error(e) => e.to_string(),
                    Value::Empty => String::new(),
                };
                format!("{shown}   ({ms:.1} ms)")
            }
            Err(e) => format!("Parse error: {e}"),
        });
    }

    /// Keyboard handling for the grid. Returns true if a repaint is needed.
    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.focus != Focus::Grid {
            return;
        }
        let (
            up,
            down,
            left,
            right,
            enter,
            tab,
            shift_tab,
            delete,
            page_up,
            page_down,
            home,
            end,
            f2,
            undo,
            redo,
            typed,
        ) = ctx.input(|i| {
            let ctrl = i.modifiers.command;
            let shift = i.modifiers.shift;
            // First printable character typed goes straight into the cell.
            let typed = i.events.iter().find_map(|e| match e {
                egui::Event::Text(t) if !t.is_empty() && !ctrl => Some(t.clone()),
                _ => None,
            });
            (
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Tab) && !shift,
                i.key_pressed(Key::Tab) && shift,
                i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace),
                i.key_pressed(Key::PageUp),
                i.key_pressed(Key::PageDown),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
                i.key_pressed(Key::F2),
                ctrl && i.key_pressed(Key::Z) && !shift,
                ctrl && (i.key_pressed(Key::Y) || (shift && i.key_pressed(Key::Z))),
                typed,
            )
        });

        let page = self.viewport_rows().saturating_sub(1).max(1) as i64;

        if undo {
            if let Some(cell) = self.wb.undo() {
                self.selection = cell;
                self.scroll_to_selection();
                self.status = format!("Undo {} · {} edits", cell.to_a1(), self.wb.edit_count());
                self.sync_formula_bar();
            }
            return;
        }
        if redo {
            if let Some(cell) = self.wb.redo() {
                self.selection = cell;
                self.scroll_to_selection();
                self.status = format!("Redo {} · {} edits", cell.to_a1(), self.wb.edit_count());
                self.sync_formula_bar();
            }
            return;
        }

        if up {
            self.move_selection(-1, 0);
        } else if down || enter {
            self.move_selection(1, 0);
        } else if left {
            self.move_selection(0, -1);
        } else if right || tab {
            self.move_selection(0, 1);
        } else if shift_tab {
            self.move_selection(0, -1);
        } else if page_up {
            self.move_selection(-page, 0);
        } else if page_down {
            self.move_selection(page, 0);
        } else if home {
            let dc = -(self.selection.col as i64);
            self.move_selection(0, dc);
        } else if end {
            let view = self.wb.view();
            let dc = view.col_count().saturating_sub(1) as i64 - self.selection.col as i64;
            self.move_selection(0, dc);
        } else if delete {
            let cell = self.selection;
            self.wb.commit_edit(cell, "");
            self.status = format!("{} cleared", cell.to_a1());
            self.sync_formula_bar();
        } else if f2 {
            let cell = self.selection;
            self.begin_edit(cell, None);
        } else if let Some(t) = typed {
            // Typing replaces the cell, matching Excel.
            let cell = self.selection;
            self.begin_edit(cell, Some(t));
        }
    }
}

impl eframe::App for FerrixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_load();
        if self.loading {
            ctx.request_repaint();
        }
        let frame_start = std::time::Instant::now();

        self.handle_keys(ctx);

        // --- toolbar ---
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::none().fill(Theme::PANEL).inner_margin(8.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("FERRIX")
                            .color(Theme::ACCENT)
                            .strong()
                            .size(15.0),
                    );
                    ui.add_space(12.0);
                    if ui.button("Open CSV…").clicked() {
                        self.open_dialog();
                    }
                    ui.add_space(4.0);
                    if ui
                        .add_enabled(self.wb.can_undo(), egui::Button::new("↶ Undo"))
                        .clicked()
                    {
                        if let Some(c) = self.wb.undo() {
                            self.selection = c;
                            self.sync_formula_bar();
                        }
                    }
                    if ui
                        .add_enabled(self.wb.can_redo(), egui::Button::new("↷ Redo"))
                        .clicked()
                    {
                        if let Some(c) = self.wb.redo() {
                            self.selection = c;
                            self.sync_formula_bar();
                        }
                    }
                    if self.loading {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new("parsing…").color(Theme::TEXT_DIM));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(s) = &self.stats {
                            let edits = self.wb.edit_count();
                            let label = if edits > 0 {
                                format!(
                                    "{} rows × {} cols · {edits} edits",
                                    fmt_int(s.rows),
                                    s.cols
                                )
                            } else {
                                format!("{} rows × {} cols", fmt_int(s.rows), s.cols)
                            };
                            ui.label(RichText::new(label).color(Theme::TEXT_DIM).size(12.0));
                        }
                    });
                });
            });

        // --- formula bar ---
        egui::TopBottomPanel::top("formula_bar")
            .frame(
                egui::Frame::none()
                    .fill(Theme::HEADER_BG)
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(self.selection.to_a1())
                            .color(Theme::ACCENT)
                            .monospace()
                            .size(13.0),
                    );
                    ui.separator();
                    ui.label(RichText::new("fx").color(Theme::TEXT_DIM).italics());

                    let resp = ui.add_sized(
                        [ui.available_width() * 0.5, 22.0],
                        egui::TextEdit::singleline(&mut self.formula_input)
                            .hint_text("=SUM(E1:E10000000)")
                            .font(egui::TextStyle::Monospace),
                    );
                    if resp.gained_focus() {
                        self.focus = Focus::FormulaBar;
                    }
                    if resp.changed() {
                        self.recompute_formula();
                    }
                    // Enter in the formula bar commits it to the selected cell.
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        let cell = self.selection;
                        let text = self.formula_input.clone();
                        let report = self.wb.commit_edit(cell, &text);
                        self.status = if let Some(e) = &report.parse_error {
                            format!("{}: {e}", cell.to_a1())
                        } else if report.circular {
                            format!("{}: circular reference", cell.to_a1())
                        } else {
                            format!(
                                "{} committed · {} recalculated ({} µs)",
                                cell.to_a1(),
                                report.recalculated,
                                report.micros
                            )
                        };
                        self.focus = Focus::Grid;
                        self.sync_formula_bar();
                    }

                    if let Some(r) = &self.formula_result {
                        let color = if r.starts_with("Parse error") || r.starts_with('#') {
                            Theme::ERROR
                        } else {
                            Theme::NUMBER
                        };
                        ui.label(RichText::new(format!("= {r}")).color(color).monospace());
                    }
                });
            });

        // --- status bar ---
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::none()
                    .fill(Theme::HEADER_BG)
                    .inner_margin(egui::Margin::symmetric(8.0, 4.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&self.status)
                            .color(Theme::TEXT_DIM)
                            .size(11.5),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let fps = if self.frame_ms > 0.0 {
                            1000.0 / self.frame_ms
                        } else {
                            0.0
                        };
                        let color = if fps >= 55.0 {
                            Theme::NUMBER
                        } else if fps >= 30.0 {
                            Theme::TEXT_DIM
                        } else {
                            Theme::ERROR
                        };
                        ui.label(
                            RichText::new(format!(
                                "{:.0} fps · {:.2} ms · {} cells",
                                fps, self.frame_ms, self.last_painted
                            ))
                            .color(color)
                            .size(11.5)
                            .monospace(),
                        );
                    });
                });
            });

        // --- grid ---
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Theme::BG))
            .show(ctx, |ui| {
                if self.wb.view().row_count() == 0 {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("Open a CSV to get started")
                                .color(Theme::TEXT_DIM)
                                .size(16.0),
                        );
                    });
                    return;
                }

                let outer = ui.available_rect_before_wrap();
                self.last_viewport_h = outer.height() - crate::grid::HEADER_HEIGHT;

                let resp = {
                    let view = self.wb.view();
                    Grid {
                        view: &view,
                        selection: Some(self.selection),
                        col_widths: &self.col_widths,
                        scroll: &mut self.scroll,
                        editing: self.editing,
                    }
                    .show(ui)
                };

                self.last_painted = resp.painted_cells;

                if let Some(cell) = resp.clicked {
                    if self.editing.is_some() && self.editing != Some(cell) {
                        self.commit_edit();
                    }
                    self.selection = cell;
                    self.focus = Focus::Grid;
                    self.sync_formula_bar();
                }
                if let Some(cell) = resp.double_clicked {
                    self.begin_edit(cell, None);
                }

                // --- in-cell editor, overlaid exactly on the cell ---
                if let Some(cell) = self.editing {
                    if let Some(rect) =
                        Grid::cell_screen_rect(cell, outer, &self.scroll, &self.col_widths)
                    {
                        let id = egui::Id::new("cell_editor");
                        let mut child = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect.expand(1.0))
                                .layout(Layout::left_to_right(Align::Center)),
                        );
                        let edit = child.add_sized(
                            rect.size(),
                            egui::TextEdit::singleline(&mut self.edit_buffer)
                                .id(id)
                                .font(egui::TextStyle::Monospace)
                                .margin(egui::Margin::symmetric(4.0, 2.0)),
                        );
                        if self.just_started_edit {
                            edit.request_focus();
                            // Put the caret at the end of any seeded text.
                            if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
                                let end =
                                    egui::text::CCursor::new(self.edit_buffer.chars().count());
                                state
                                    .cursor
                                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                                state.store(ctx, id);
                            }
                            self.just_started_edit = false;
                        }
                        let enter = child.input(|i| i.key_pressed(Key::Enter));
                        let esc = child.input(|i| i.key_pressed(Key::Escape));
                        let tab = child.input(|i| i.key_pressed(Key::Tab));
                        if esc {
                            self.cancel_edit();
                        } else if enter {
                            self.commit_edit();
                            self.move_selection(1, 0);
                        } else if tab {
                            self.commit_edit();
                            self.move_selection(0, 1);
                        }
                    } else {
                        // Scrolled out of view — commit rather than lose input.
                        self.commit_edit();
                    }
                }
            });

        let ms = frame_start.elapsed().as_secs_f64() as f32 * 1000.0;
        self.frame_ms = if self.frame_ms == 0.0 {
            ms
        } else {
            self.frame_ms * 0.9 + ms * 0.1
        };
    }
}

/// Size each column from a sample of its contents. We sample rather than scan
/// so opening a huge file stays instant.
fn compute_col_widths(sheet: &Sheet) -> Vec<f32> {
    const SAMPLE: usize = 200;
    let rows = sheet.row_count();
    let step = (rows / SAMPLE).max(1);

    (0..sheet.col_count())
        .map(|c| {
            let mut widest = sheet.header_or_letter(c).len();
            let mut r = 0;
            while r < rows {
                let len = sheet.display(CellRef::new(r as u32, c as u32)).len();
                widest = widest.max(len);
                r += step;
            }
            let w = widest as f32 * 7.2 + 20.0;
            if w.is_finite() {
                w.clamp(64.0, 320.0)
            } else {
                DEFAULT_COL_WIDTH
            }
        })
        .collect()
}

/// Thousands separators for readability in the status bar.
fn fmt_int(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::Value;

    #[test]
    fn int_formatting() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1000), "1,000");
        assert_eq!(fmt_int(200_000_000), "200,000,000");
    }

    #[test]
    fn col_widths_are_bounded_and_sampled() {
        let mut s = Sheet::new("t");
        for r in 0..1000u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
            s.set_text(CellRef::new(r, 1), "a-fairly-long-text-value-here");
        }
        let w = compute_col_widths(&s);
        assert_eq!(w.len(), 2);
        for width in &w {
            assert!((64.0..=320.0).contains(width), "width {width} out of range");
        }
        assert!(w[1] > w[0]);
    }
}
