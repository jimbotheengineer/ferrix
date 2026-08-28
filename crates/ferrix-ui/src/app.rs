//! Application state and top-level layout.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use egui::{Align, Key, Layout, RichText};
use ferrix_core::{CellRef, Selection, Sheet, Value};
use ferrix_formula::{eval_view, parse};
use ferrix_io::{load_csv, CsvOptions};

use crate::grid::{Grid, ScrollState, DEFAULT_COL_WIDTH};
use crate::sheet_view::BaseData;
use crate::theme::Theme;
use crate::workbook::Workbook;

/// What a background load produced.
struct Loaded {
    base: BaseData,
    rows: usize,
    cols: usize,
    /// Human-readable summary for the status bar.
    summary: String,
    col_widths: Vec<f32>,
    /// Where edits for this dataset are saved, and the identity of the base
    /// they belong to. `None` when the source could not be fingerprinted.
    edits_path: Option<PathBuf>,
    fingerprint: Option<ferrix_io::edits::BaseFingerprint>,
    /// Edits restored from a sidecar, if one was present and current.
    restored: Option<ferrix_core::EditOverlay>,
    /// Set when a sidecar existed but was rejected, so the UI can warn instead
    /// of silently discarding the user's saved work.
    edit_warning: Option<String>,
}

type LoadResult = Result<Loaded, String>;

/// Progress reported from the loader thread.
#[derive(Clone, Copy, Default)]
struct Progress {
    done: u64,
    total: u64,
}

/// Where keyboard input should go.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Grid,
    Search,
    /// Editing in-cell; the buffer holds what has been typed so far.
    Cell,
    FormulaBar,
}

pub struct FerrixApp {
    wb: Workbook,
    stats_rows: usize,
    stats_cols: usize,
    col_widths: Vec<f32>,
    /// Active selection. `cursor` is the cell that typing lands in.
    selection: Selection,
    scroll: ScrollState,

    focus: Focus,
    editing: Option<CellRef>,
    edit_buffer: String,
    /// True on the frame an edit begins, so we can grab keyboard focus once.
    just_started_edit: bool,

    formula_input: String,
    formula_result: Option<String>,

    /// Search state. `results` is kept sorted row-major so the grid can
    /// binary-search the visible slice each frame.
    search_open: bool,
    search_input: String,
    search_results: ferrix_core::SearchResults,
    search_index: usize,
    search_focus_pending: bool,

    /// Selection a fill drag started from, and the live target while dragging.
    fill_source: Option<Selection>,
    fill_target: Option<Selection>,

    /// Where to persist edits, and the base identity they belong to. Both are
    /// None until a file is loaded.
    edits_path: Option<PathBuf>,
    fingerprint: Option<ferrix_io::edits::BaseFingerprint>,

    status: String,
    loading: bool,
    load_rx: Option<Receiver<LoadResult>>,
    progress_rx: Option<Receiver<Progress>>,
    progress: Progress,
    frame_ms: f32,
    last_painted: usize,
    /// Height of the grid body last frame, used for page-up/down sizing.
    last_viewport_h: f32,
}

impl FerrixApp {
    pub fn new(initial: Option<PathBuf>) -> Self {
        let mut app = Self {
            wb: Workbook::new(BaseData::Memory(Sheet::new("Sheet1"))),
            stats_rows: 0,
            stats_cols: 0,
            col_widths: Vec::new(),
            selection: Selection::default(),
            scroll: ScrollState::default(),
            focus: Focus::Grid,
            editing: None,
            edit_buffer: String::new(),
            just_started_edit: false,
            formula_input: String::new(),
            formula_result: None,
            search_open: false,
            search_input: String::new(),
            search_results: ferrix_core::SearchResults::default(),
            search_index: 0,
            search_focus_pending: false,
            fill_source: None,
            fill_target: None,
            edits_path: None,
            fingerprint: None,
            status: "Ready — open a CSV to begin".into(),
            loading: false,
            load_rx: None,
            progress_rx: None,
            progress: Progress::default(),
            frame_ms: 0.0,
            last_painted: 0,
            last_viewport_h: 600.0,
        };
        if let Some(p) = initial {
            app.start_load(p);
        }
        app
    }

    /// Kick off a load on a worker thread so the UI never blocks — converting
    /// a 10GB file takes minutes and the window must stay responsive.
    fn start_load(&mut self, path: PathBuf) {
        let (tx, rx) = channel();
        let (ptx, prx) = channel::<Progress>();
        self.loading = true;
        self.progress = Progress::default();
        self.status = format!("Opening {}…", path.display());

        std::thread::spawn(move || {
            let result = load_any(&path, |done, total| {
                let _ = ptx.send(Progress { done, total });
            });
            let _ = tx.send(result);
        });
        self.load_rx = Some(rx);
        self.progress_rx = Some(prx);
    }

    fn poll_load(&mut self) {
        // Drain progress first so the bar is current even on a slow frame.
        if let Some(prx) = &self.progress_rx {
            while let Ok(p) = prx.try_recv() {
                self.progress = p;
            }
        }

        let Some(rx) = &self.load_rx else { return };
        match rx.try_recv() {
            Ok(Ok(loaded)) => {
                self.col_widths = loaded.col_widths;
                self.status = loaded.summary;
                self.stats_rows = loaded.rows;
                self.stats_cols = loaded.cols;
                self.edits_path = loaded.edits_path;
                self.fingerprint = loaded.fingerprint;
                let restored_count = loaded.restored.as_ref().map(|o| o.len());
                self.wb = match loaded.restored {
                    Some(ov) => Workbook::new(loaded.base).with_overlay(ov),
                    None => Workbook::new(loaded.base),
                };
                // Formula cells were saved with their source; rebuild the
                // dependency graph and recompute so cached values cannot drift
                // from a base that may have been recalculated elsewhere.
                if restored_count.is_some() {
                    self.wb.rebuild_graph_and_recalc();
                }
                if let Some(w) = loaded.edit_warning {
                    self.status = format!("Saved edits not applied — {w}");
                } else if let Some(n) = restored_count {
                    self.status = format!("{} · restored {} saved edits", self.status, fmt_int(n));
                }
                self.selection.move_to(CellRef::new(0, 0));
                self.scroll = ScrollState::default();
                self.loading = false;
                self.load_rx = None;
                self.progress_rx = None;
                self.sync_formula_bar();
            }
            Ok(Err(e)) => {
                self.status = format!("Load failed: {e}");
                self.loading = false;
                self.load_rx = None;
                self.progress_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.status = "Load thread died".into();
                self.loading = false;
                self.load_rx = None;
                self.progress_rx = None;
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
    ///
    /// `extend` is Shift held: the anchor stays put and only the cursor moves,
    /// growing the range. Otherwise the selection collapses to one cell.
    fn move_selection_ext(&mut self, drow: i64, dcol: i64, extend: bool) {
        if self.editing.is_some() {
            self.commit_edit();
        }
        let view = self.wb.view();
        let max_row = view.row_count().saturating_sub(1) as i64;
        let max_col = view.col_count().saturating_sub(1) as i64;
        let r = (self.selection.cursor.row as i64 + drow).clamp(0, max_row.max(0));
        let c = (self.selection.cursor.col as i64 + dcol).clamp(0, max_col.max(0));
        let target = CellRef::new(r as u32, c as u32);
        if extend {
            self.selection.extend_to(target);
        } else {
            self.selection.move_to(target);
        }
        self.scroll_to_selection();
        self.sync_formula_bar();
    }

    fn move_selection(&mut self, drow: i64, dcol: i64) {
        self.move_selection_ext(drow, dcol, false);
    }

    /// Largest block a clipboard or clear operation will touch.
    ///
    /// A user can select an entire 200M-row column; turning that into text
    /// would exhaust memory long before it finished. The limit is generous
    /// enough for any realistic paste and small enough to stay instant.
    const MAX_BLOCK_CELLS: u64 = 1_000_000;

    /// Copy the selection to the system clipboard as TSV.
    fn copy_selection(&mut self, ctx: &egui::Context, cut: bool) {
        let sel = self.selection;
        let Some(block) = self.wb.copy_block(sel, Self::MAX_BLOCK_CELLS) else {
            self.status = format!(
                "{} cells is too many to copy (limit {})",
                fmt_int(sel.cell_count() as usize),
                fmt_int(Self::MAX_BLOCK_CELLS as usize)
            );
            return;
        };
        let tsv = ferrix_core::tsv::to_tsv(&block);
        let n = sel.cell_count();
        ctx.copy_text(tsv);
        if cut {
            match self.wb.clear_range(sel, Self::MAX_BLOCK_CELLS) {
                Ok(cleared) => {
                    self.status = format!(
                        "Cut {} cells ({} cleared)",
                        fmt_int(n as usize),
                        fmt_int(cleared)
                    );
                    self.sync_formula_bar();
                }
                Err(e) => self.status = e,
            }
        } else {
            self.status = format!("Copied {} cells to clipboard", fmt_int(n as usize));
        }
    }

    /// Paste TSV from the clipboard at the selection's top-left corner.
    fn paste_clipboard(&mut self, text: &str) {
        let block = ferrix_core::tsv::from_tsv(text);
        if block.is_empty() {
            self.status = "Clipboard is empty".into();
            return;
        }
        let origin = self.selection.bounds().0;
        match self.wb.paste_block(origin, &block, Self::MAX_BLOCK_CELLS) {
            Ok(n) => {
                // Select what was pasted, so the user sees the affected region
                // and can undo or overwrite it in one gesture.
                let rows = block.len() as u32;
                let cols = block.iter().map(|r| r.len()).max().unwrap_or(0) as u32;
                self.selection = Selection::new(
                    origin,
                    CellRef::new(
                        origin.row + rows.saturating_sub(1),
                        origin.col + cols.saturating_sub(1),
                    ),
                );
                self.status = format!("Pasted {} cells", fmt_int(n));
                self.sync_formula_bar();
            }
            Err(e) => self.status = e,
        }
    }

    /// Clear every cell in the selection as one undo step.
    fn clear_selection(&mut self) {
        let sel = self.selection;
        match self.wb.clear_range(sel, Self::MAX_BLOCK_CELLS) {
            Ok(0) => self.status = "Nothing to clear".into(),
            Ok(n) => {
                self.status = format!("Cleared {} cells · {}", fmt_int(n), sel.label());
                self.sync_formula_bar();
            }
            Err(e) => self.status = e,
        }
    }

    /// Select the whole used range (Ctrl+A).
    fn select_all(&mut self) {
        let view = self.wb.view();
        let rows = view.row_count().saturating_sub(1) as u32;
        let cols = view.col_count().saturating_sub(1) as u32;
        self.selection = Selection::new(CellRef::new(0, 0), CellRef::new(rows, cols));
        self.status = format!(
            "Selected {} · {} cells",
            self.selection.label(),
            fmt_int(self.selection.cell_count() as usize)
        );
    }

    /// Keep the selected cell on screen after keyboard navigation.
    fn scroll_to_selection(&mut self) {
        let visible = (self.viewport_rows() as f64 - 1.0).max(1.0);
        let row = self.selection.cursor.row as f64;
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

    /// Persist edits to the sidecar beside the base file.
    ///
    /// Cheap by construction: only the overlay is written, so saving a handful
    /// of edits over a 200M-row dataset writes a handful of kilobytes and
    /// never touches the base.
    fn save_edits(&mut self) {
        let (Some(path), Some(fp)) = (self.edits_path.clone(), self.fingerprint) else {
            self.status = "Nothing to save — no file is open".into();
            return;
        };
        if self.wb.overlay.is_empty() && !self.wb.is_dirty() {
            self.status = "No edits to save".into();
            return;
        }
        let t = std::time::Instant::now();
        match ferrix_io::edits::save_edits(&path, &self.wb.overlay, fp) {
            Ok(bytes) => {
                self.wb.mark_saved();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.status = format!(
                    "Saved {} edit{} ({} bytes) to {} in {:.1} ms",
                    fmt_int(self.wb.overlay.len()),
                    if self.wb.overlay.len() == 1 { "" } else { "s" },
                    fmt_int(bytes as usize),
                    name,
                    t.elapsed().as_secs_f64() * 1000.0
                );
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn sync_formula_bar(&mut self) {
        self.formula_input = self.wb.view().edit_text(self.selection.cursor);
        self.recompute_formula();
    }

    /// Re-run the search for the current input.
    ///
    /// Cheap enough to call on every keystroke even at 200M rows, because the
    /// engine matches the needle against the string arena first and only then
    /// scans columns as integers.
    fn run_search(&mut self) {
        const LIMIT: usize = 100_000;
        let Some(query) = ferrix_core::Query::new(self.search_input.trim(), false, false) else {
            self.search_results = ferrix_core::SearchResults::default();
            self.search_index = 0;
            return;
        };
        let view = self.wb.view();
        self.search_results = view.search(&query, LIMIT);
        // Resume from wherever the cursor is rather than jumping to the top.
        self.search_index = self.search_results.index_at_or_after(self.selection.cursor);
        self.jump_to_current_match();
    }

    /// Move the selection to the active match and scroll it into view.
    fn jump_to_current_match(&mut self) {
        if let Some(cell) = self.search_results.wrapped(self.search_index) {
            self.selection.move_to(cell);
            self.center_on_selection();
            self.formula_input = self.wb.view().edit_text(cell);
        }
    }

    fn next_match(&mut self) {
        if self.search_results.matches.is_empty() {
            return;
        }
        self.search_index = (self.search_index + 1) % self.search_results.matches.len();
        self.jump_to_current_match();
    }

    fn prev_match(&mut self) {
        if self.search_results.matches.is_empty() {
            return;
        }
        let n = self.search_results.matches.len();
        self.search_index = (self.search_index + n - 1) % n;
        self.jump_to_current_match();
    }

    fn close_search(&mut self) {
        self.search_open = false;
        self.search_results = ferrix_core::SearchResults::default();
        self.search_index = 0;
        self.focus = Focus::Grid;
    }

    /// Centre the viewport on the selection — used when jumping to a match, so
    /// the hit lands mid-screen rather than scraping the edge.
    fn center_on_selection(&mut self) {
        let visible = (self.last_viewport_h / crate::grid::ROW_HEIGHT) as f64;
        let target = self.selection.cursor.row as f64 - visible / 2.0;
        self.scroll.row_offset = target.max(0.0);
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
        // Ctrl+F works from anywhere, including while the search box has focus
        // (where it re-focuses and selects, matching browser behaviour).
        let (ctrl_f, ctrl_s, escape, f3, shift_f3) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::F),
                i.modifiers.command && i.key_pressed(Key::S),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::F3) && !i.modifiers.shift,
                i.key_pressed(Key::F3) && i.modifiers.shift,
            )
        });
        if ctrl_s {
            self.save_edits();
            return;
        }
        // Clipboard and select-all only apply when the grid owns the keyboard;
        // inside a text field the widget's own handling must win.
        let grid_has_keys = ctx.memory(|m| m.focused()).is_none() && self.focus == Focus::Grid;
        if grid_has_keys {
            let (copy, cut, paste, select_all) = ctx.input(|i| {
                let c = i.modifiers.command;
                (
                    c && i.key_pressed(Key::C),
                    c && i.key_pressed(Key::X),
                    c && i.key_pressed(Key::V),
                    c && i.key_pressed(Key::A),
                )
            });
            if select_all {
                self.select_all();
                return;
            }
            if copy || cut {
                self.copy_selection(ctx, cut);
                return;
            }
            if paste {
                // egui delivers clipboard content as a Paste event rather than
                // exposing a read API, so take whatever arrived this frame.
                let text = ctx.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Paste(t) => Some(t.clone()),
                        _ => None,
                    })
                });
                if let Some(t) = text {
                    self.paste_clipboard(&t);
                }
                return;
            }
        }
        if ctrl_f {
            self.search_open = true;
            self.focus = Focus::Search;
            self.search_focus_pending = true;
            return;
        }
        if self.search_open {
            if escape {
                self.close_search();
                return;
            }
            if f3 {
                self.next_match();
                return;
            }
            if shift_f3 {
                self.prev_match();
                return;
            }
        }

        // Gate grid keys on egui's REAL keyboard focus, not our own flag.
        //
        // `self.focus` is app-level bookkeeping; egui separately tracks which
        // widget owns the keyboard. When a TextEdit (search box, formula bar)
        // holds focus, characters must not also reach the grid. Without this,
        // typing "consulting" into the search box simultaneously drove the
        // grid's type-to-edit path and cleared a cell — silent data loss.
        let widget_has_keyboard = ctx.memory(|m| m.focused()).is_some();
        if widget_has_keyboard || self.focus != Focus::Grid {
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
            shift_held,
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
                shift,
            )
        });

        let page = self.viewport_rows().saturating_sub(1).max(1) as i64;

        if undo {
            if let Some(cell) = self.wb.undo() {
                self.selection.move_to(cell);
                self.scroll_to_selection();
                self.status = format!("Undo {} · {} edits", cell.to_a1(), self.wb.edit_count());
                self.sync_formula_bar();
            }
            return;
        }
        if redo {
            if let Some(cell) = self.wb.redo() {
                self.selection.move_to(cell);
                self.scroll_to_selection();
                self.status = format!("Redo {} · {} edits", cell.to_a1(), self.wb.edit_count());
                self.sync_formula_bar();
            }
            return;
        }

        // Shift+Arrow extends the selection from the anchor; a bare arrow
        // collapses it. Tab/Enter always collapse, matching Excel.
        let ext = shift_held;
        if up {
            self.move_selection_ext(-1, 0, ext);
        } else if down {
            self.move_selection_ext(1, 0, ext);
        } else if enter {
            self.move_selection(1, 0);
        } else if left {
            self.move_selection_ext(0, -1, ext);
        } else if right {
            self.move_selection_ext(0, 1, ext);
        } else if tab {
            self.move_selection(0, 1);
        } else if shift_tab {
            self.move_selection(0, -1);
        } else if page_up {
            self.move_selection_ext(-page, 0, ext);
        } else if page_down {
            self.move_selection_ext(page, 0, ext);
        } else if home {
            let dc = -(self.selection.cursor.col as i64);
            self.move_selection_ext(0, dc, ext);
        } else if end {
            let view = self.wb.view();
            let dc = view.col_count().saturating_sub(1) as i64 - self.selection.cursor.col as i64;
            self.move_selection_ext(0, dc, ext);
        } else if delete {
            // Deletes the whole selection as ONE undo step, not one per cell.
            self.clear_selection();
        } else if f2 {
            let cell = self.selection.cursor;
            self.begin_edit(cell, None);
        } else if let Some(t) = typed {
            // Typing replaces the cell, matching Excel.
            let cell = self.selection.cursor;
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
                    let dirty = self.wb.is_dirty();
                    if ui
                        .add_enabled(
                            dirty && self.edits_path.is_some(),
                            egui::Button::new(if dirty { "💾 Save*" } else { "💾 Save" }),
                        )
                        .on_hover_text("Save edits (Ctrl+S)")
                        .clicked()
                    {
                        self.save_edits();
                    }
                    if ui
                        .add_enabled(self.wb.can_undo(), egui::Button::new("↶ Undo"))
                        .clicked()
                    {
                        if let Some(c) = self.wb.undo() {
                            self.selection.move_to(c);
                            self.sync_formula_bar();
                        }
                    }
                    if ui
                        .add_enabled(self.wb.can_redo(), egui::Button::new("↷ Redo"))
                        .clicked()
                    {
                        if let Some(c) = self.wb.redo() {
                            self.selection.move_to(c);
                            self.sync_formula_bar();
                        }
                    }
                    if self.loading {
                        ui.add(egui::Spinner::new().size(14.0));
                        // A 10GB conversion takes minutes, so show real
                        // progress rather than an indefinite spinner.
                        if self.progress.total > 0 {
                            let frac = self.progress.done as f32 / self.progress.total as f32;
                            ui.add(egui::ProgressBar::new(frac).desired_width(180.0).text(
                                format!(
                                    "{:.0}%  ({:.1}/{:.1} GB)",
                                    frac * 100.0,
                                    self.progress.done as f64 / 1e9,
                                    self.progress.total as f64 / 1e9
                                ),
                            ));
                        } else {
                            ui.label(RichText::new("opening…").color(Theme::TEXT_DIM));
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.stats_rows > 0 {
                            let edits = self.wb.edit_count();
                            let mut label = format!(
                                "{} rows × {} cols",
                                fmt_int(self.stats_rows),
                                self.stats_cols
                            );
                            if self.wb.base.is_mapped() {
                                label.push_str(" · mmap");
                            }
                            if edits > 0 {
                                label.push_str(&format!(" · {edits} edits"));
                            }
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
                        RichText::new(self.selection.label())
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
                        let cell = self.selection.cursor;
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

        // --- search bar ---
        if self.search_open {
            egui::TopBottomPanel::top("search_bar")
                .frame(
                    egui::Frame::none()
                        .fill(Theme::HEADER_BG)
                        .inner_margin(egui::Margin::symmetric(8.0, 6.0)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("🔍").size(13.0));
                        let resp = ui.add_sized(
                            [260.0, 22.0],
                            egui::TextEdit::singleline(&mut self.search_input)
                                .hint_text("Find in sheet…")
                                .font(egui::TextStyle::Monospace),
                        );
                        if self.search_focus_pending {
                            resp.request_focus();
                            self.search_focus_pending = false;
                        }
                        if resp.gained_focus() {
                            self.focus = Focus::Search;
                        }
                        // Live search as the user types.
                        if resp.changed() {
                            self.run_search();
                        }
                        // Enter advances; Shift+Enter goes back.
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            if ui.input(|i| i.modifiers.shift) {
                                self.prev_match();
                            } else {
                                self.next_match();
                            }
                            resp.request_focus();
                        }

                        if ui
                            .button("◀")
                            .on_hover_text("Previous (Shift+Enter)")
                            .clicked()
                        {
                            self.prev_match();
                        }
                        if ui.button("▶").on_hover_text("Next (Enter / F3)").clicked() {
                            self.next_match();
                        }

                        let r = &self.search_results;
                        if self.search_input.trim().is_empty() {
                            ui.label(
                                RichText::new("type to search")
                                    .color(Theme::TEXT_DIM)
                                    .size(11.5),
                            );
                        } else if r.total == 0 {
                            ui.label(RichText::new("no matches").color(Theme::ERROR).size(11.5));
                        } else {
                            let shown = format!(
                                "{} of {}{}",
                                self.search_index + 1,
                                fmt_int(r.total),
                                if r.truncated { " (capped)" } else { "" }
                            );
                            ui.label(RichText::new(shown).color(Theme::TEXT).size(11.5));
                            // Surfacing why it was fast: N distinct strings
                            // matched, not N cells compared.
                            ui.label(
                                RichText::new(format!(
                                    "· {} ms · {} distinct string{} matched",
                                    r.millis,
                                    r.matched_strings,
                                    if r.matched_strings == 1 { "" } else { "s" }
                                ))
                                .color(Theme::TEXT_DIM)
                                .size(11.0),
                            );
                        }

                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("✕").on_hover_text("Close (Esc)").clicked() {
                                self.close_search();
                            }
                        });
                    });
                });
        }

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
                        matches: &self.search_results.matches,
                        filling: self.fill_source.is_some(),
                        current_match: if self.search_open {
                            self.search_results.wrapped(self.search_index)
                        } else {
                            None
                        },
                    }
                    .show(ui)
                };

                self.last_painted = resp.painted_cells;

                if let Some(cell) = resp.clicked {
                    if self.editing.is_some() && self.editing != Some(cell) {
                        self.commit_edit();
                    }
                    // Shift+click extends from the existing anchor, like every
                    // other spreadsheet and file list.
                    if ui.input(|i| i.modifiers.shift) {
                        self.selection.extend_to(cell);
                    } else {
                        self.selection.move_to(cell);
                    }
                    self.focus = Focus::Grid;
                    self.sync_formula_bar();
                }
                // Drag extends without moving the anchor, so a press-and-sweep
                // paints a range. Ignored while editing, where a drag is text
                // selection inside the cell editor.
                if let Some(cell) = resp.drag_to {
                    if self.editing.is_none() && cell != self.selection.cursor {
                        self.selection.extend_to(cell);
                        self.status = format!(
                            "{} · {} cells",
                            self.selection.label(),
                            fmt_int(self.selection.cell_count() as usize)
                        );
                    }
                }
                // --- fill handle ---
                if resp.fill_started {
                    self.fill_source = Some(self.selection);
                    self.fill_target = Some(self.selection);
                }
                if let (Some(src), Some(cell)) = (self.fill_source, resp.fill_to) {
                    // Grow along the dominant axis only, so a slightly
                    // diagonal drag still does something predictable.
                    let (tl, br) = src.bounds();
                    let drow = if cell.row > br.row {
                        cell.row as i64 - br.row as i64
                    } else if cell.row < tl.row {
                        cell.row as i64 - tl.row as i64
                    } else {
                        0
                    };
                    let dcol = if cell.col > br.col {
                        cell.col as i64 - br.col as i64
                    } else if cell.col < tl.col {
                        cell.col as i64 - tl.col as i64
                    } else {
                        0
                    };
                    let target = match ferrix_formula::fill::fill_direction(drow, dcol) {
                        Some(ferrix_formula::fill::FillDir::Down) => {
                            Selection::new(tl, CellRef::new(cell.row, br.col))
                        }
                        Some(ferrix_formula::fill::FillDir::Up) => {
                            Selection::new(CellRef::new(cell.row, tl.col), br)
                        }
                        Some(ferrix_formula::fill::FillDir::Right) => {
                            Selection::new(tl, CellRef::new(br.row, cell.col))
                        }
                        Some(ferrix_formula::fill::FillDir::Left) => {
                            Selection::new(CellRef::new(tl.row, cell.col), br)
                        }
                        None => src,
                    };
                    self.fill_target = Some(target);
                    // Preview the region the fill will cover.
                    self.selection = target;
                }
                if resp.fill_released {
                    if let (Some(src), Some(tgt)) = (self.fill_source, self.fill_target) {
                        match self.wb.fill_range(src, tgt, Self::MAX_BLOCK_CELLS) {
                            Ok((0, _)) => {}
                            Ok((n, kind)) => {
                                self.status = format!(
                                    "Filled {} cells ({}) · {}",
                                    fmt_int(n),
                                    match kind {
                                        ferrix_formula::fill::FillKind::Series => "series",
                                        ferrix_formula::fill::FillKind::Copy => "copy",
                                    },
                                    tgt.label()
                                );
                                self.sync_formula_bar();
                            }
                            Err(e) => {
                                self.status = e;
                                self.selection = src;
                            }
                        }
                    }
                    self.fill_source = None;
                    self.fill_target = None;
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

/// Open a file, choosing storage based on size.
///
/// Small files parse straight into RAM. Large ones are converted once into the
/// columnar `.ferrix` format beside the source and then memory-mapped, so the
/// dataset is bounded by disk rather than memory and later opens are instant.
fn load_any<F>(path: &Path, mut progress: F) -> LoadResult
where
    F: FnMut(u64, u64),
{
    if !ferrix_io::should_use_mmap(path) {
        let (sheet, stats) = load_csv(path, CsvOptions::default()).map_err(|e| e.to_string())?;
        let widths = compute_col_widths_mem(&sheet);
        let summary = format!(
            "Loaded {} rows × {} cols in {} ms ({:.0} MB/s) · {:.2} GB resident",
            fmt_int(stats.rows),
            stats.cols,
            stats.parse_millis,
            stats.throughput_mbps(),
            sheet.heap_bytes() as f64 / 1e9,
        );
        let rows = sheet.row_count();
        let cols = sheet.col_count();
        let (edits_path, fingerprint, restored, edit_warning) =
            restore_edits(path, rows as u64, cols as u32);
        return Ok(Loaded {
            rows,
            cols,
            col_widths: widths,
            summary,
            base: BaseData::Memory(sheet),
            edits_path,
            fingerprint,
            restored,
            edit_warning,
        });
    }

    // Large file: use (or build) the columnar cache next to the source.
    let cache = ferrix_io::cache_path_for(path);
    let reused = ferrix_io::cache_is_fresh(path, &cache);
    let mut convert_note = String::new();

    if !reused {
        let stats = ferrix_io::convert_csv(path, &cache, b',', true, &mut progress)
            .map_err(|e| e.to_string())?;
        convert_note = format!(
            "converted in {:.0}s at {:.0} MB/s · ",
            stats.millis as f64 / 1000.0,
            stats.throughput_mbps()
        );
    }

    let mut mapped = ferrix_io::MappedSheet::open(&cache).map_err(|e| e.to_string())?;
    // Recover header names from the source's first line; the cache stores data
    // only, and re-reading one line is instant even for a 10GB file.
    if let Some(h) = read_header_line(path) {
        mapped.set_headers(h);
    }

    let widths = compute_col_widths_mapped(&mapped);
    let summary = format!(
        "{}{} rows × {} cols · {:.1} GB mapped from disk{}",
        convert_note,
        fmt_int(mapped.row_count()),
        mapped.col_count(),
        mapped.mapped_bytes() as f64 / 1e9,
        if reused { " (cached)" } else { "" }
    );

    let rows = mapped.row_count();
    let cols = mapped.col_count();
    // Edits are keyed to the cache, not the CSV: the cache is what the grid
    // actually reads, and regenerating it is exactly the event that should
    // invalidate saved edits.
    let (edits_path, fingerprint, restored, edit_warning) =
        restore_edits(&cache, rows as u64, cols as u32);

    Ok(Loaded {
        rows,
        cols,
        col_widths: widths,
        summary,
        base: BaseData::Mapped(Box::new(mapped)),
        edits_path,
        fingerprint,
        restored,
        edit_warning,
    })
}

/// Look for a sidecar next to `base` and load it if it belongs to this data.
///
/// Returns the sidecar path and fingerprint regardless, so a later save knows
/// where to write even when nothing was restored.
fn restore_edits(
    base: &Path,
    rows: u64,
    cols: u32,
) -> (
    Option<PathBuf>,
    Option<ferrix_io::edits::BaseFingerprint>,
    Option<ferrix_core::EditOverlay>,
    Option<String>,
) {
    use ferrix_io::edits;
    let fp = match edits::BaseFingerprint::of(base, rows, cols) {
        Ok(f) => f,
        // Cannot fingerprint (permissions, vanished file): saving would be
        // unsafe, so report no path rather than risk a mismatched sidecar.
        Err(_) => return (None, None, None, None),
    };
    let path = edits::edits_path_for(base);
    match edits::load_edits(&path, fp) {
        Ok(Some(ov)) => (Some(path), Some(fp), Some(ov), None),
        Ok(None) => (Some(path), Some(fp), None, None),
        // A rejected sidecar must be surfaced. Silently continuing would look
        // like the user's saved edits simply vanished.
        Err(e) => (Some(path), Some(fp), None, Some(e.to_string())),
    }
}

/// Read just the first line of a CSV for column headers.
fn read_header_line(path: &Path) -> Option<Vec<String>> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    Some(
        line.trim_end()
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .collect(),
    )
}

/// Size columns from a sample. Sampling rather than scanning is what keeps
/// opening a 200M-row file instant.
fn compute_col_widths_mem(sheet: &Sheet) -> Vec<f32> {
    const SAMPLE: usize = 200;
    let rows = sheet.row_count();
    let step = (rows / SAMPLE).max(1);
    (0..sheet.col_count())
        .map(|c| {
            let mut widest = sheet.header_or_letter(c).len();
            let mut r = 0;
            while r < rows {
                widest = widest.max(sheet.display(CellRef::new(r as u32, c as u32)).len());
                r += step;
            }
            width_for(widest)
        })
        .collect()
}

fn compute_col_widths_mapped(m: &ferrix_io::MappedSheet) -> Vec<f32> {
    const SAMPLE: usize = 200;
    let rows = m.row_count();
    let step = (rows / SAMPLE).max(1);
    (0..m.col_count())
        .map(|c| {
            let mut widest = m.header_or_letter(c).len();
            let mut r = 0;
            while r < rows {
                widest = widest.max(m.display(CellRef::new(r as u32, c as u32)).len());
                r += step;
            }
            width_for(widest)
        })
        .collect()
}

fn width_for(chars: usize) -> f32 {
    let w = chars as f32 * 7.2 + 20.0;
    if w.is_finite() {
        w.clamp(64.0, 320.0)
    } else {
        DEFAULT_COL_WIDTH
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::{Sheet, Value};

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
        let w = compute_col_widths_mem(&s);
        assert_eq!(w.len(), 2);
        for width in &w {
            assert!((64.0..=320.0).contains(width), "width {width} out of range");
        }
        assert!(w[1] > w[0]);
    }

    #[test]
    fn width_is_clamped_at_both_ends() {
        // A one-character column must not collapse; a huge one must not
        // push every other column off screen.
        assert_eq!(width_for(0), 64.0);
        assert_eq!(width_for(10_000), 320.0);
    }
}
