//! Application state and top-level layout.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use egui::{Align, Key, Layout, RichText};
use ferrix_core::{CellRef, Selection, Sheet, Value};
use ferrix_formula::{eval_view, parse};
use ferrix_io::{load_csv, CsvOptions};

use crate::grid::{Grid, ScrollState, DEFAULT_COL_WIDTH};
use crate::prefs::Prefs;
use crate::sheet_view::BaseData;
use crate::theme::{Theme, ThemeMode};
use crate::workbook::Workbook;

/// What a background load produced.
struct Loaded {
    base: BaseData,
    /// Name for the first sheet — the source's own sheet name for xlsx, or
    /// the file stem otherwise.
    sheet_name: String,
    /// Every OTHER sheet in the source, in workbook order. Empty for CSV.
    /// Each carries its own independent base and formula overlay, so a
    /// workbook can mix a mmap'd sheet with small in-RAM ones.
    extra_sheets: Vec<(String, BaseData, ferrix_core::EditOverlay)>,
    /// Formulas belonging to the FIRST sheet, when the source had any.
    first_formulas: Option<ferrix_core::EditOverlay>,
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
    /// Match options. The engine has always supported both; these expose them.
    search_case_sensitive: bool,
    search_whole_cell: bool,
    /// Filter mode: when on, the grid renders only rows containing a match.
    search_filter_mode: bool,
    /// The visible-row -> underlying-row mapping backing filter mode.
    ///
    /// Rebuilt once per search (and once when the toggle flips), never per
    /// frame. `None` whenever filter mode is off.
    row_filter: Option<ferrix_core::RowFilter>,

    /// Chart panel state: the built scene, its annotations, and the window.
    chart: crate::chart_panel::ChartPanel,

    /// Display column being dragged by its header, between press and release.
    header_drag: Option<usize>,

    /// Selection a fill drag started from, and the live target while dragging.
    fill_source: Option<Selection>,
    fill_target: Option<Selection>,

    /// Structured tables defined over the current sheet.
    ///
    /// Only the first is decorated today — the grid takes one `TableDecor` —
    /// but the vector is what xlsx import hands over and what export needs
    /// back, so it is stored whole rather than collapsed to an Option.
    tables: Vec<ferrix_core::Table>,
    /// Row mask from the active header filters, recomputed only when a filter
    /// changes rather than per frame.
    table_mask: Option<ferrix_core::RowMask>,
    /// Per-column uniqueness indexes, rebuilt alongside the mask. A `Unique`
    /// rule cannot be judged from one cell, so this is the one thing the
    /// renderer cannot compute locally.
    table_uniques: Vec<Option<ferrix_core::UniquenessIndex>>,
    /// Standing validation report for the badge in the status bar.
    table_report: ferrix_core::ValidationReport,

    /// Where to persist edits, and the base identity they belong to. Both are
    /// None until a file is loaded.
    edits_path: Option<PathBuf>,
    fingerprint: Option<ferrix_io::edits::BaseFingerprint>,

    status: String,
    loading: bool,
    load_rx: Option<Receiver<LoadResult>>,
    progress_rx: Option<Receiver<Progress>>,
    progress: Progress,
    /// Stops the loader/converter. Cleared when the load finishes.
    load_cancel: Option<ferrix_core::CancelToken>,

    /// A CSV export running on a worker thread. There is at most one: a
    /// second would compete for disk and confuse the progress bar.
    exporting: bool,
    export_rx: Option<Receiver<Result<ferrix_io::export::ExportStats, String>>>,
    export_progress_rx: Option<Receiver<Progress>>,
    export_progress: Progress,
    export_cancel: Option<ferrix_core::CancelToken>,
    export_path: Option<PathBuf>,
    export_started: Option<std::time::Instant>,

    /// Memory measurement shown in the status bar, refreshed once a second by
    /// the frame loop rather than sampled per paint.
    budget: ferrix_core::Budget,
    /// Worker threads rayon is actually running, for the same status line.
    /// Configured rayon worker count. Read at startup to size the pool;
    /// kept so a future settings UI can show and change it.
    #[allow(dead_code)]
    worker_threads: usize,
    frame_ms: f32,
    last_painted: usize,
    /// Height of the grid body last frame, used for page-up/down sizing.
    last_viewport_h: f32,

    /// True while the unsaved-changes confirmation is on screen. Closing the
    /// window with a dirty workbook is the last silent data-loss path in the
    /// app, so the close request is intercepted and the user gets a choice.
    close_prompt: bool,
    /// Set once the user has resolved the prompt, so the follow-up close
    /// request is allowed straight through rather than re-prompting forever.
    allow_close: bool,

    /// Sheet currently being renamed inline, and the buffer being typed into.
    renaming: Option<ferrix_core::SheetId>,
    rename_buffer: String,
    rename_focus_pending: bool,
    /// Sheet being dragged along the tab strip, for reordering.
    dragging_tab: Option<ferrix_core::SheetId>,

    /// The active palette (issue #19). Held as a value and passed down to
    /// every painter, so the toggle switches the entire UI at once.
    theme: Theme,
    /// Whether the user has ever picked a theme. Until they do we follow the
    /// OS, and keep following it if it changes mid-session.
    theme_chosen: bool,
    /// Show empty rows past the end of the sheet (issue #20).
    show_empty_rows: bool,
    /// Persisted preferences, written back whenever a toggle flips.
    prefs: Prefs,
}

// The observation API below (row_count, display, cursor, ...) is consumed only
// by the test harness, so a release build sees it as dead. It is deliberately
// part of the app rather than the harness: the harness must ask the REAL app
// what happened, never keep its own copy of the state.
#[allow(dead_code)]
impl FerrixApp {
    pub fn new(initial: Option<PathBuf>) -> Self {
        let prefs = Prefs::load();
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
            search_case_sensitive: false,
            search_whole_cell: false,
            chart: crate::chart_panel::ChartPanel::default(),
            search_filter_mode: false,
            row_filter: None,
            header_drag: None,
            fill_source: None,
            fill_target: None,
            tables: Vec::new(),
            table_mask: None,
            table_uniques: Vec::new(),
            table_report: ferrix_core::ValidationReport::default(),
            edits_path: None,
            fingerprint: None,
            status: "Ready — open a CSV to begin".into(),
            loading: false,
            load_rx: None,
            progress_rx: None,
            progress: Progress::default(),
            load_cancel: None,
            exporting: false,
            export_rx: None,
            export_progress_rx: None,
            export_progress: Progress::default(),
            export_cancel: None,
            export_path: None,
            export_started: None,
            budget: ferrix_core::Budget::sample(),
            worker_threads: ferrix_io::pool::configured_threads(),
            frame_ms: 0.0,
            last_painted: 0,
            last_viewport_h: 600.0,
            close_prompt: false,
            allow_close: false,
            renaming: None,
            rename_buffer: String::new(),
            rename_focus_pending: false,
            dragging_tab: None,
            // A saved preference wins. Without one we start dark and let the
            // first frame adopt the OS preference — egui only knows it once a
            // frame has run, so it cannot be read here.
            theme: Theme::of(prefs.theme.unwrap_or_default()),
            theme_chosen: prefs.theme.is_some(),
            show_empty_rows: prefs.show_empty_rows,
            prefs,
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
        let cancel = ferrix_core::CancelToken::new();
        let mut should_cancel = cancel.checker();
        self.loading = true;
        self.progress = Progress::default();
        self.status = format!("Opening {}…", path.display());

        std::thread::spawn(move || {
            let result = load_any(
                &path,
                |done, total| {
                    let _ = ptx.send(Progress { done, total });
                },
                &mut should_cancel,
            );
            let _ = tx.send(result);
        });
        self.load_rx = Some(rx);
        self.progress_rx = Some(prx);
        self.load_cancel = Some(cancel);
    }

    /// Ask a running load/conversion to stop.
    ///
    /// The converter polls between 32 MB blocks and deletes its scratch
    /// directory and partial cache on the way out, so a cancelled conversion
    /// leaves no half-written `.ferrix` that a later open could trust.
    fn cancel_load(&mut self) {
        if let Some(c) = &self.load_cancel {
            c.cancel();
            self.status = "Cancelling…".into();
        }
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
                self.wb = build_workbook(
                    loaded.base,
                    loaded.sheet_name,
                    loaded.first_formulas,
                    loaded.restored,
                    loaded.extra_sheets,
                );
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
                self.load_cancel = None;
                self.sync_formula_bar();
            }
            Ok(Err(e)) => {
                // A cancel is a normal outcome the user asked for, not a
                // failure to apologise for.
                self.status = if e.contains("cancelled") {
                    "Load cancelled — nothing was written".to_string()
                } else {
                    format!("Load failed: {e}")
                };
                self.loading = false;
                self.load_rx = None;
                self.progress_rx = None;
                self.load_cancel = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.status = "Load thread died".into();
                self.loading = false;
                self.load_rx = None;
                self.progress_rx = None;
                self.load_cancel = None;
            }
        }
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Spreadsheets", &["csv", "tsv", "txt", "xlsx"])
            .add_filter("CSV", &["csv", "tsv", "txt"])
            .add_filter("Excel", &["xlsx"])
            .pick_file()
        {
            self.start_load(path);
        }
    }

    fn begin_edit(&mut self, cell: CellRef, seed: Option<String>) {
        // A cell covered by a merge holds no value of its own — the anchor
        // does. Redirect the edit there rather than refusing it outright, so
        // typing into a merged block edits the thing the user can see instead
        // of silently doing nothing.
        //
        // This is the single chokepoint for every edit path (typing, F2,
        // double-click), which is why the check lives here and not in three
        // call sites that could drift apart.
        let cell = self.wb.merges.resolve(cell);
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
    /// The active palette, for the startup styling pass in `main`.
    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Switch theme and remember the choice.
    ///
    /// Persisting on the toggle rather than on exit means the preference
    /// survives a crash or a kill, which "save on shutdown" does not.
    fn set_theme(&mut self, mode: ThemeMode) {
        self.theme = Theme::of(mode);
        // An explicit choice stops the OS-following behaviour for good.
        self.theme_chosen = true;
        self.prefs.theme = Some(mode);
        self.persist_prefs();
        self.status = format!("{} theme", mode.as_str());
    }

    fn set_show_empty_rows(&mut self, on: bool) {
        self.show_empty_rows = on;
        self.prefs.show_empty_rows = on;
        self.persist_prefs();
        self.status = if on {
            format!(
                "Showing {} empty rows past the end — they are not counted as data \
                 until you type in one",
                crate::grid::EMPTY_ROW_PADDING
            )
        } else {
            "Empty rows hidden".to_string()
        };
    }

    /// Write preferences out, best effort. Failing to save a toggle is not
    /// worth a modal; it goes to the status bar and nowhere else.
    fn persist_prefs(&mut self) {
        if let Err(e) = self.prefs.save() {
            self.status = format!("Preference not saved: {e}");
        }
    }

    /// The padding rows currently on offer, or `None` when the toggle is off.
    ///
    /// `first_pad_screen_row` is the count of rows the FILTERS resolve, so
    /// padding always begins after the last row either filter kept, and
    /// `first_pad_data_row` is one past the end of the whole sheet — never
    /// past the filtered subset, which would alias onto hidden records.
    fn pad_space(&self) -> Option<crate::grid::PadSpace> {
        if !self.show_empty_rows {
            return None;
        }
        let view = self.wb.view();
        let data_rows = view.row_count().max(1);
        let filtered = match &self.row_filter {
            Some(f) => f.len(),
            None => match (self.tables.first(), &self.table_mask) {
                (Some(_), Some(m)) => m.visible_rows(),
                _ => data_rows,
            },
        };
        Some(crate::grid::PadSpace {
            first_pad_screen_row: filtered,
            first_pad_data_row: view.row_count(),
        })
    }

    /// Last row the cursor may move to with the keyboard.
    ///
    /// With the toggle on this reaches into the padding, so arrowing down off
    /// the end of a short file lands somewhere typeable. It is a NAVIGATION
    /// bound only: `row_count` is untouched, so export, SUM and the status bar
    /// still see the real sheet.
    fn max_navigable_row(&self) -> i64 {
        let rows = self.wb.view().row_count();
        let pad = if self.show_empty_rows {
            crate::grid::EMPTY_ROW_PADDING
        } else {
            0
        };
        (rows + pad).saturating_sub(1) as i64
    }

    fn move_selection_ext(&mut self, drow: i64, dcol: i64, extend: bool) {
        if self.editing.is_some() {
            self.commit_edit();
        }
        let view = self.wb.view();
        // Navigation may reach into the empty padding when the toggle is on;
        // the sheet's own extent is unchanged.
        let max_row = self.max_navigable_row();
        let max_col = view.col_count().saturating_sub(1) as i64;
        // Vertical movement is in VISIBLE rows under a filter: pressing Down
        // must land on the next row the user can actually see, not on a hidden
        // neighbour. The result is converted straight back to an underlying
        // row, so every downstream consumer keeps working in real addresses.
        let r = match (&self.row_filter, drow) {
            (Some(f), d) if d != 0 && !f.is_empty() => {
                let here = f
                    .visible_of(self.selection.cursor.row)
                    .unwrap_or_else(|| f.visible_at_or_after(self.selection.cursor.row))
                    as i64;
                let target = (here + d).clamp(0, f.len() as i64 - 1);
                f.underlying(target as usize).unwrap_or(0) as i64
            }
            _ => (self.selection.cursor.row as i64 + drow).clamp(0, max_row.max(0)),
        };
        let c = (self.selection.cursor.col as i64 + dcol).clamp(0, max_col.max(0));
        let target = CellRef::new(r as u32, c as u32);
        if target != self.selection.cursor {
            // Moving off a cell ends its coalescing run, so coming back later
            // is a separate undo step rather than folding into the old one.
            self.wb.end_edit_run();
        }
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

    /// Largest block a clipboard or clear operation will touch, derived from
    /// the memory actually available right now.
    ///
    /// A user can select an entire 200M-row column; turning that into text
    /// would exhaust memory long before it finished. This used to be a flat
    /// one million cells — a number that was an OOM kill on a small machine
    /// and an insult on a large one. It is now whatever fits the measured
    /// budget at [`cost::CLIPBOARD_CELL`] per cell, which on a machine with
    /// room comes to tens of millions and on a starved one falls back to the
    /// budget floor rather than to zero.
    ///
    /// Sampled per call rather than cached: the answer legitimately changes
    /// when the user opens something else, and a clipboard operation is not
    /// hot enough for a syscall to matter.
    fn max_block_cells(&self) -> u64 {
        ferrix_core::Budget::sample().max_units(ferrix_core::budget::cost::CLIPBOARD_CELL)
    }

    /// Cap for operations that WRITE cells into the overlay (paste, fill).
    ///
    /// Distinct from [`Self::max_block_cells`] because an overlay cell is far
    /// more expensive than a clipboard string: a hash-map entry, a
    /// `CellInput`, and both before/after copies for undo.
    fn max_overlay_cells(&self) -> u64 {
        ferrix_core::Budget::sample().max_units(ferrix_core::budget::cost::OVERLAY_CELL)
    }

    /// Copy the selection to the system clipboard as TSV.
    fn copy_selection(&mut self, ctx: &egui::Context, cut: bool) {
        let sel = self.selection;
        let limit = self.max_block_cells();
        let Some(block) = self.wb.copy_block(sel, limit) else {
            self.status = format!(
                "{} cells is too many to copy — {} fit in the memory available now",
                fmt_int(sel.cell_count() as usize),
                fmt_int(limit as usize)
            );
            return;
        };
        let tsv = ferrix_core::tsv::to_tsv(&block);
        let n = sel.cell_count();
        ctx.copy_text(tsv);
        if cut {
            let write_limit = self.max_overlay_cells();
            match self.wb.clear_range(sel, write_limit) {
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
        let limit = self.max_overlay_cells();
        match self.wb.paste_block(origin, &block, limit) {
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
        let limit = self.max_overlay_cells();
        match self.wb.clear_range(sel, limit) {
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
    ///
    /// `scroll.row_offset` counts VISIBLE rows, which under a filter are not
    /// the same as underlying rows — so the cursor's row is mapped first. A
    /// cursor on a filtered-out row has no visible position and the viewport
    /// is left alone rather than jumping somewhere arbitrary.
    fn scroll_to_selection(&mut self) {
        let visible = (self.viewport_rows() as f64 - 1.0).max(1.0);
        let Some(row) = self.visible_row_of(self.selection.cursor.row) else {
            return;
        };
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
    /// Returns true if edits were actually written to disk.
    fn save_edits(&mut self) -> bool {
        let (Some(path), Some(fp)) = (self.edits_path.clone(), self.fingerprint) else {
            self.status = "Nothing to save — no file is open".into();
            return false;
        };
        if self.wb.overlay.is_empty() && !self.wb.is_dirty() {
            self.status = "No edits to save".into();
            return false;
        }
        let t = std::time::Instant::now();
        match ferrix_io::edits::save_edits(&path, &self.wb.overlay, fp) {
            Ok(bytes) => {
                // Undo history does not survive a save (see README, "Editing").
                // Say so out loud: silently dropping it is exactly the kind of
                // thing that makes a user distrust the undo button.
                let lost = self.wb.save_committed();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let history = if lost > 0 {
                    format!(
                        " · undo history cleared ({} step{})",
                        fmt_int(lost),
                        if lost == 1 { "" } else { "s" }
                    )
                } else {
                    String::new()
                };
                self.status = format!(
                    "Saved {} edit{} ({} bytes) to {} in {:.1} ms{}",
                    fmt_int(self.wb.overlay.len()),
                    if self.wb.overlay.len() == 1 { "" } else { "s" },
                    fmt_int(bytes as usize),
                    name,
                    t.elapsed().as_secs_f64() * 1000.0,
                    history
                );
                true
            }
            Err(e) => {
                self.status = format!("Save failed: {e}");
                false
            }
        }
    }

    /// The unsaved-changes modal shown when the user tries to close the window
    /// with a dirty workbook.
    ///
    /// Three honest options: Save (write the sidecar, then close), Discard
    /// (close and lose the edits — stated plainly), and Cancel (stay put).
    /// If saving fails, the close is NOT allowed to proceed: the status bar
    /// carries the error and the user keeps their data.
    fn show_close_prompt(&mut self, ctx: &egui::Context) {
        let th = self.theme;
        let mut save_and_close = false;
        let mut discard = false;
        let mut cancel = false;

        egui::Window::new("Unsaved changes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let edits = self.wb.edit_count();
                ui.label(
                    RichText::new(format!(
                        "{} unsaved edit{} will be lost.",
                        fmt_int(edits),
                        if edits == 1 { "" } else { "s" }
                    ))
                    .size(13.5),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let can_save = self.edits_path.is_some();
                    if ui
                        .add_enabled(can_save, egui::Button::new("💾 Save and close"))
                        .clicked()
                    {
                        save_and_close = true;
                    }
                    if ui.button("Discard and close").clicked() {
                        discard = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if self.edits_path.is_none() {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("No file is open, so there is nowhere to save these edits.")
                            .color(th.text_dim)
                            .size(11.5),
                    );
                }
            });

        // Escape cancels, matching every other dialog in the app.
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            cancel = true;
        }

        if cancel {
            self.close_prompt = false;
            return;
        }
        if save_and_close {
            if self.save_edits() {
                self.close_prompt = false;
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // Save failed: leave the prompt up with the error in the status
            // bar rather than closing over the top of lost work.
            return;
        }
        if discard {
            self.close_prompt = false;
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Build a chart from the current selection and open the panel.
    ///
    /// A single cell is almost never what someone means by "chart this", so a
    /// lone cursor is widened to the whole column — the common case being
    /// "click a column header, chart it".
    fn open_chart(&mut self) {
        let sel = if self.selection.is_single() {
            let c = self.selection.cursor;
            let last = self.stats_rows.saturating_sub(1) as u32;
            Selection::new(CellRef::new(0, c.col), CellRef::new(last, c.col))
        } else {
            self.selection
        };

        let kind = self.chart.kind;
        {
            let view = self.wb.view();
            self.chart.build(&view, sel, kind);
        }
        self.chart.open = true;
        self.status = self.chart.status.clone();
    }

    /// Rebuild the chart from its stored source range, after a kind change.
    fn rebuild_chart(&mut self) {
        if let Some(sel) = self.chart.source {
            let kind = self.chart.kind;
            {
                let view = self.wb.view();
                self.chart.build(&view, sel, kind);
            }
            self.status = self.chart.status.clone();
        }
    }

    /// Write the current chart, annotations included, to an SVG file.
    fn export_chart_svg(&mut self) {
        let Some(svg) = self.chart.to_svg(1200.0, 600.0) else {
            self.status = "No chart to export".to_string();
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("SVG", &["svg"])
            .set_file_name("chart.svg")
            .save_file()
        else {
            return;
        };
        self.status = match std::fs::write(&path, svg) {
            Ok(()) => format!(
                "Chart exported → {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Err(e) => format!("Chart export failed: {e}"),
        };
    }

    /// The chart window: controls, canvas, annotation list.
    fn show_chart_window(&mut self, ctx: &egui::Context) {
        if !self.chart.open {
            return;
        }
        let th = self.theme;
        let mut open = self.chart.open;
        let mut rebuild = false;
        let mut export = false;

        egui::Window::new("Chart")
            .open(&mut open)
            .default_size([760.0, 480.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for k in crate::chart_panel::ChartKind::ALL {
                        if ui
                            .selectable_label(self.chart.kind == k, k.label())
                            .clicked()
                            && self.chart.kind != k
                        {
                            self.chart.kind = k;
                            rebuild = true;
                        }
                    }
                    ui.separator();
                    let placing = self.chart.placing_note;
                    if ui
                        .selectable_label(placing, "📌 Note")
                        .on_hover_text("Click the chart to place a note")
                        .clicked()
                    {
                        self.chart.placing_note = !placing;
                    }
                    if ui
                        .button("⬈ SVG…")
                        .on_hover_text("Export as a resizable vector image")
                        .clicked()
                    {
                        export = true;
                    }
                });

                ui.label(
                    egui::RichText::new(&self.chart.status)
                        .size(11.0)
                        .color(th.text_dim),
                );
                ui.separator();

                let avail = ui.available_size();
                let canvas = egui::vec2(avail.x, (avail.y - 60.0).max(160.0));
                let (rect, _) = ui.allocate_exact_size(canvas, egui::Sense::hover());

                if let Some(scene) = self.chart.scene.clone() {
                    // The chart's chrome follows the app theme; its exported
                    // SVG deliberately does not. See SVG_FOLLOWS_APP_THEME.
                    let (vp, resp) = crate::chart_panel::paint_scene(
                        ui,
                        &scene,
                        &self.chart.annotations,
                        rect,
                        th,
                    );

                    // Place a note where the user clicks, in DATA coordinates,
                    // so it stays put when the window is resized.
                    if self.chart.placing_note {
                        if let Some(pos) = resp.interact_pointer_pos() {
                            if resp.clicked() {
                                let n = crate::chart_panel::note_at(&vp, pos, "note");
                                let i = self.chart.annotations.add(n);
                                self.chart.editing_note = Some(i);
                                self.chart.note_buffer = "note".to_string();
                                self.chart.placing_note = false;
                            }
                        }
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Select a range and choose a chart type")
                                .color(th.text_dim),
                        );
                    });
                }

                // Annotation editor.
                if let Some(i) = self.chart.editing_note {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Note:");
                        let r = ui.text_edit_singleline(&mut self.chart.note_buffer);
                        if r.changed() {
                            if let Some(a) = self.chart.annotations.get_mut(i) {
                                a.text = self.chart.note_buffer.clone();
                            }
                        }
                        if ui.button("Done").clicked() {
                            self.chart.editing_note = None;
                        }
                        if ui.button("Delete").clicked() {
                            self.chart.annotations.remove(i);
                            self.chart.editing_note = None;
                        }
                    });
                } else if !self.chart.annotations.is_empty() {
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} note(s):",
                                self.chart.annotations.len()
                            ))
                            .size(11.0)
                            .color(th.text_dim),
                        );
                        let texts: Vec<(usize, String)> = self
                            .chart
                            .annotations
                            .iter()
                            .enumerate()
                            .map(|(i, a)| (i, a.text.clone()))
                            .collect();
                        for (i, t) in texts {
                            if ui.small_button(&t).clicked() {
                                self.chart.editing_note = Some(i);
                                self.chart.note_buffer = t;
                            }
                        }
                    });
                }
            });

        self.chart.open = open;
        if rebuild {
            self.rebuild_chart();
        }
        if export {
            self.export_chart_svg();
        }
    }

    /// Export the current sheet — base plus edits — to a CSV the rest of the
    /// world can read.
    ///
    /// ## No row limit any more
    ///
    /// This used to refuse anything above five million rows, because the
    /// export ran on the UI thread and a 200M-row job froze the window for
    /// well over two minutes. The refusal was the right stopgap and the wrong
    /// permanent answer: `export_csv` already streams with memory bounded by
    /// the widest row rather than the row count, and already polls a cancel
    /// closure. The only thing missing was a way to read the sheet off-thread.
    ///
    /// [`OwnedSheet`](crate::sheet_view::OwnedSheet) supplies that — the base
    /// shared by `Arc`, the sparse overlay copied — so the export now runs on
    /// a worker with live progress and a working Cancel button, and the row
    /// cap is gone. What is still checked is the SNAPSHOT cost, which is the
    /// only unbounded allocation on this path.
    fn export_dialog(&mut self) {
        if self.exporting {
            self.status = "An export is already running — cancel it first".into();
            return;
        }

        // The one allocation an export makes that scales with user input: the
        // overlay copy. The base is shared, and the writer's buffers are fixed.
        let cost = crate::sheet_view::OwnedSheet::snapshot_cost_bytes(&self.wb.overlay);
        let budget = ferrix_core::Budget::sample();
        if let Err(msg) = budget.admit(cost, "Exporting this sheet's edits") {
            self.status = msg;
            return;
        }

        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name("export.csv")
            .save_file()
        else {
            return;
        };

        let snapshot = crate::sheet_view::OwnedSheet::new(
            std::sync::Arc::clone(&self.wb.base),
            &self.wb.overlay,
        );
        let rows = snapshot.row_count();

        let (tx, rx) = channel::<Result<ferrix_io::export::ExportStats, String>>();
        let (ptx, prx) = channel::<Progress>();
        let cancel = ferrix_core::CancelToken::new();
        let mut should_cancel = cancel.checker();
        let target = path.clone();

        std::thread::spawn(move || {
            let result = ferrix_io::export::export_csv(
                &target,
                &snapshot,
                ferrix_io::export::ExportOptions::default(),
                |done, total| {
                    let _ = ptx.send(Progress {
                        done: done as u64,
                        total: total as u64,
                    });
                },
                &mut should_cancel,
            )
            .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });

        self.exporting = true;
        self.export_cancel = Some(cancel);
        self.export_rx = Some(rx);
        self.export_progress_rx = Some(prx);
        self.export_progress = Progress {
            done: 0,
            total: rows as u64,
        };
        self.export_path = Some(path);
        self.export_started = Some(std::time::Instant::now());
        self.status = format!("Exporting {} rows…", fmt_int(rows));
    }

    /// Ask a running export to stop.
    ///
    /// The worker polls the token every 50,000 rows, deletes its temp file,
    /// and returns `Cancelled` — so any pre-existing file at the destination
    /// is left exactly as it was.
    fn cancel_export(&mut self) {
        if let Some(c) = &self.export_cancel {
            c.cancel();
            self.status = "Cancelling export…".into();
        }
    }

    /// Drain export progress and completion. Mirrors `poll_load`.
    fn poll_export(&mut self) {
        if let Some(prx) = &self.export_progress_rx {
            while let Ok(p) = prx.try_recv() {
                // The worker reports rows done against rows total; keep the
                // total we computed if the worker has not sent one yet.
                self.export_progress.done = p.done;
                if p.total > 0 {
                    self.export_progress.total = p.total;
                }
            }
        }

        let Some(rx) = &self.export_rx else { return };
        let finished = match rx.try_recv() {
            Ok(result) => Some(match result {
                Ok(stats) => {
                    let secs = self
                        .export_started
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(0.0);
                    let name = self
                        .export_path
                        .as_ref()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .unwrap_or_default();
                    format!(
                        "Exported {} rows × {} cols ({:.1} MB) in {:.1}s ({:.0} MB/s) → {}",
                        fmt_int(stats.rows),
                        stats.cols,
                        stats.bytes as f64 / 1e6,
                        secs,
                        if secs > 0.0 {
                            (stats.bytes as f64 / 1e6) / secs
                        } else {
                            0.0
                        },
                        name
                    )
                }
                // Cancellation is a normal outcome, not a failure: say so
                // plainly, and say that nothing was overwritten.
                Err(e) if e.contains("cancelled") => {
                    "Export cancelled — no file was written or overwritten".to_string()
                }
                Err(e) => format!("Export failed: {e}"),
            }),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some("Export thread died — the destination file is unchanged".to_string())
            }
        };

        if let Some(status) = finished {
            self.status = status;
            self.exporting = false;
            self.export_rx = None;
            self.export_progress_rx = None;
            self.export_cancel = None;
            self.export_path = None;
            self.export_started = None;
        }
    }

    /// Open an `.xlsx`, bringing across any Excel Tables defined in it.
    ///
    /// Values and table definitions are read in separate passes because they
    /// live in different parts of the package: calamine reads the cells,
    /// [`ferrix_io::import_tables`] reads `xl/tables/*.xml` plus the
    /// worksheet's validation and conditional-format elements.
    fn open_xlsx_dialog(&mut self) {
        self.open_xlsx_dialog_impl()
    }

    /// Set the selection, for the headless harness only.
    #[cfg(test)]
    pub fn set_selection_for_test(&mut self, a: CellRef, b: CellRef) {
        // Selection::new carries the cursor, so there is nothing else to set.
        self.selection = Selection::new(a, b);
    }

    /// Merge the current selection, or unmerge if it already covers merges.
    ///
    /// One button for both directions because that is how the user thinks
    /// about it — the selection either is merged or it is not.
    pub fn toggle_merge(&mut self) {
        let (a, b) = self.selection.bounds();
        let range = ferrix_core::TableRange::new(a.row, a.col, b.row, b.col);

        let existing = self.wb.merges.unmerge_range(range);
        if existing > 0 {
            self.wb.mark_dirty();
            self.status = format!(
                "Unmerged {existing} region{}",
                if existing == 1 { "" } else { "s" }
            );
            return;
        }
        match self.wb.merges.merge(range) {
            Ok(()) => {
                self.wb.mark_dirty();
                self.status = "Merged".into();
            }
            // A refusal is reported, never silent: the user pressed a button
            // and must learn why nothing happened.
            Err(e) => self.status = format!("Cannot merge: {e}"),
        }
    }

    /// Font family, size, and the B/I/U toggles.
    ///
    /// The toggles are three-state underneath but two-state to the user: a
    /// mixed selection shows unpressed, and pressing it sets the whole
    /// selection on. That is what people expect from a word processor, and it
    /// means a selection is never left in a state the button cannot express.
    fn type_controls(&mut self, ui: &mut egui::Ui, th: crate::theme::Theme) {
        let cur = self.selection_typography();
        let res = cur.resolved(12.5);

        // Family. A closed set, so an unavailable font can never silently
        // change how a saved sheet looks on another machine.
        let fam_label = match res.family {
            ferrix_core::format::FontFamily::Monospace => "Mono",
            ferrix_core::format::FontFamily::Proportional => "Sans",
        };
        egui::ComboBox::from_id_salt("font_family")
            .selected_text(fam_label)
            .width(64.0)
            .show_ui(ui, |ui| {
                for (fam, label) in [
                    (ferrix_core::format::FontFamily::Proportional, "Sans"),
                    (ferrix_core::format::FontFamily::Monospace, "Mono"),
                ] {
                    if ui.selectable_label(res.family == fam, label).clicked() {
                        self.apply_typography(|t| {
                            t.family = Some(fam);
                        });
                    }
                }
            });

        // Size. Clamped in core, so no path can produce an unrenderable sheet.
        let mut pt = res.size;
        let resp = ui.add(
            egui::DragValue::new(&mut pt)
                .speed(0.5)
                .range(ferrix_core::format::MIN_FONT_PT..=ferrix_core::format::MAX_FONT_PT)
                .suffix(" pt"),
        );
        if resp.changed() {
            let clamped = ferrix_core::format::clamp_font_pt(pt);
            self.apply_typography(move |t| {
                t.size = Some(clamped);
            });
        }

        let toggle = |ui: &mut egui::Ui, on: bool, label: &str, hover: &str| -> bool {
            let text = if on {
                RichText::new(label).color(th.accent).strong()
            } else {
                RichText::new(label)
            };
            ui.selectable_label(on, text).on_hover_text(hover).clicked()
        };

        if toggle(ui, res.bold, "B", "Bold (Ctrl+B)") {
            let on = !res.bold;
            self.apply_typography(move |t| {
                t.bold = Some(on);
            });
        }
        if toggle(ui, res.italic, "I", "Italic (Ctrl+I)") {
            let on = !res.italic;
            self.apply_typography(move |t| {
                t.italic = Some(on);
            });
        }
        if toggle(ui, res.underline, "U", "Underline (Ctrl+U)") {
            let on = !res.underline;
            self.apply_typography(move |t| {
                t.underline = Some(on);
            });
        }
    }

    /// Typography of the cursor cell — what the toolbar reflects.
    ///
    /// Reading the cursor rather than polling the whole selection keeps this
    /// O(1): a 200M-row selection must not cost a scan just to draw a toolbar
    /// once per frame.
    pub fn selection_typography(&self) -> ferrix_core::format::Typography {
        self.wb
            .format
            .cell_override(self.cursor())
            .map(|o| o.manual.typography)
            .unwrap_or_default()
    }

    /// Apply a type change across the selection.
    ///
    /// A multi-cell selection becomes ONE range entry rather than an override
    /// per cell, so bolding a whole column is a few dozen bytes and does not
    /// depend on the row count. A single cell stays a cell override, which is
    /// what makes the common case cheap to look up while painting.
    pub fn apply_typography(&mut self, f: impl Fn(&mut ferrix_core::format::Typography)) {
        let (a, b) = self.selection.bounds();
        let single = a == b;

        if single {
            let mut ov = self.wb.format.cell_override(a).cloned().unwrap_or_default();
            f(&mut ov.manual.typography);
            self.wb.format.set_cell_override(a, ov);
        } else {
            // Seed from the cursor so a toggle over a range starts from what
            // the toolbar was showing, then apply to the whole rectangle.
            let mut ty = self.selection_typography();
            f(&mut ty);
            let range = ferrix_core::TableRange::new(a.row, a.col, b.row, b.col);
            self.wb.format.set_range_manual(
                range,
                ferrix_core::ManualStyle {
                    fill: None,
                    text: None,
                    typography: ty,
                },
            );
        }
        self.wb.mark_dirty();
        self.status = "Formatting applied".into();
    }

    fn open_xlsx_dialog_impl(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Excel workbook", &["xlsx"])
            .pick_file()
        else {
            return;
        };

        let sheets = match ferrix_io::import_xlsx(&path) {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("Open failed: {e}");
                return;
            }
        };
        let Some((name, sheet)) = sheets.into_iter().next() else {
            self.status = "Workbook has no worksheets".into();
            return;
        };
        let rows = sheet.row_count();
        let cols = sheet.col_count();

        // A table part that will not parse must not cost the user their data:
        // the sheet still opens, and the failure is reported.
        let (tables, note) = match ferrix_io::import_tables(&path) {
            Ok(t) => {
                let n = t.len();
                (
                    t.into_iter()
                        .filter(|t| t.sheet_index == 0)
                        .map(|t| t.table)
                        .collect::<Vec<_>>(),
                    format!(", {n} table(s)"),
                )
            }
            Err(e) => (Vec::new(), format!(" (table parts unreadable: {e})")),
        };

        self.wb = Workbook::new(BaseData::Memory(sheet));
        self.stats_rows = rows;
        self.stats_cols = cols;
        self.col_widths = vec![crate::grid::DEFAULT_COL_WIDTH; cols];
        self.selection = Selection::default();
        self.scroll = ScrollState::default();
        self.edits_path = None;
        self.fingerprint = None;
        self.set_tables(tables);
        self.status = format!("Opened {name}: {} rows × {cols} cols{note}", fmt_int(rows));
        self.sync_formula_bar();
    }

    /// Write this sheet and its tables as a real Excel workbook.
    fn export_xlsx_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Excel workbook", &["xlsx"])
            .set_file_name("export.xlsx")
            .save_file()
        else {
            return;
        };
        // Only the in-RAM path can be handed to the writer; a mapped base has
        // no `Sheet` to give it.
        let BaseData::Memory(sheet) = &*self.wb.base else {
            self.status =
                "xlsx export of memory-mapped data is not supported yet — export CSV instead"
                    .into();
            return;
        };
        self.status = match ferrix_io::export_xlsx_with_tables(&path, sheet, "Sheet1", &self.tables)
        {
            Ok(()) => format!(
                "Exported {} table(s) → {}",
                self.tables.len(),
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Err(e) => format!("Export failed: {e}"),
        };
    }

    /// Install structured tables over the current sheet and refresh everything
    /// derived from them.
    ///
    /// Kept as the single entry point so the filter mask, the uniqueness
    /// indexes and the validation badge can never drift out of sync with the
    /// table definitions. Public so an xlsx import can hand its tables over.
    ///
    /// Observation API for the test harness (`harness.rs`).
    ///
    /// Deliberately READ-ONLY. The harness drives the app through real input
    /// events and then asks what happened; if it could mutate state directly
    /// it would be able to fake a passing test, and a harness that can lie is
    /// worse than none. Everything here is a query with no side effects.
    pub fn row_count(&self) -> usize {
        self.stats_rows
    }

    /// Columns in the loaded sheet.
    pub fn col_count(&self) -> usize {
        self.stats_cols
    }

    /// Rendered contents of a cell, exactly as the grid would paint it —
    /// base data with any edit applied.
    pub fn display(&self, cell: CellRef) -> String {
        self.wb.view().display(cell)
    }

    /// The cursor cell — where typing lands.
    pub fn cursor(&self) -> CellRef {
        self.selection.cursor
    }

    /// Selection extent as (top-left, bottom-right).
    pub fn selection_bounds(&self) -> (CellRef, CellRef) {
        self.selection.bounds()
    }

    /// The status line, the app's own account of what it last did.
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// Whether the search bar is open.
    pub fn search_is_open(&self) -> bool {
        self.search_open
    }

    /// Number of matches from the last search.
    pub fn search_match_count(&self) -> usize {
        self.search_results.total
    }

    /// Whether the workbook has unsaved edits.
    pub fn is_dirty(&self) -> bool {
        self.wb.is_dirty()
    }

    /// Depth of the undo stack, for asserting that a bulk operation collapsed
    /// into exactly one entry.
    pub fn undo_depth(&self) -> usize {
        self.wb.undo_depth()
    }

    /// Move columns in the display order.
    ///
    /// Exposed on the app (not just the workbook) because the reorder gesture
    /// lives in the grid, and because the harness needs to drive it: dragging
    /// a header through synthetic pixels would test the drag arithmetic, not
    /// the reorder semantics this actually guards.
    pub fn move_columns(&mut self, from: u64, count: u64, to: u64) -> Result<(), String> {
        let r = self.wb.move_columns(from, count, to);
        if r.is_ok() {
            self.status = format!("Moved {count} column(s)");
        }
        r
    }

    /// Whether a load is still in flight.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn set_tables(&mut self, tables: Vec<ferrix_core::Table>) {
        self.tables = tables;
        self.refresh_tables();
    }

    /// Recompute the filter mask, uniqueness indexes and validation report.
    ///
    /// Deliberately *not* per frame. Each of these needs a pass over the
    /// table's rows, which is fine on a filter change and ruinous at 60 Hz, so
    /// it runs when the definitions or the data change and the results are
    /// cached until then.
    fn refresh_tables(&mut self) {
        self.table_mask = None;
        self.table_uniques = Vec::new();
        self.table_report = ferrix_core::ValidationReport::default();

        let Some(table) = self.tables.first() else {
            return;
        };
        // Only the in-RAM path can be filtered/validated today. A mapped base
        // exposes no per-column scan hook yet, and silently showing an
        // unfiltered view would be worse than showing no filter at all — so
        // the state stays empty and the status bar says why.
        let BaseData::Memory(sheet) = &*self.wb.base else {
            self.status = format!(
                "Table {:?} loaded; filtering and validation are not yet available on \
                 memory-mapped data",
                table.name
            );
            return;
        };

        if table.columns.iter().any(|c| c.filter.is_some()) {
            self.table_mask = Some(sheet.filter_table(table, usize::MAX));
        }
        self.table_uniques = (0..table.columns.len())
            .map(|i| sheet.uniqueness_index(table, i))
            .collect();
        // Capped: a table where every row is bad must not build a 200M-entry
        // list just to render a badge.
        self.table_report = sheet.validate_table(table, 1000);
    }

    fn sync_formula_bar(&mut self) {
        self.formula_input = self.wb.view().edit_text(self.selection.cursor);
        self.recompute_formula();
    }

    /// Save where the user is looking on the current sheet.
    fn stash_view_state(&mut self) {
        self.wb.set_view_state(crate::workbook::SheetViewState {
            scroll: self.scroll,
            selection: self.selection,
        });
    }

    /// Switch tabs, preserving each sheet's scroll position and selection.
    fn switch_sheet(&mut self, id: ferrix_core::SheetId) {
        if id == self.wb.active_sheet() {
            return;
        }
        if self.editing.is_some() {
            self.commit_edit();
        }
        // Stash BEFORE activating, so the sheet we are leaving remembers where
        // we were; then adopt whatever the sheet we are entering remembered.
        self.stash_view_state();
        if self.wb.activate(id).is_err() {
            return;
        }
        let state = self.wb.view_state();
        self.scroll = state.scroll;
        self.selection = state.selection;
        // Column widths and search hits belong to the sheet we just left.
        self.col_widths = Vec::new();
        self.search_results = ferrix_core::SearchResults::default();
        self.search_index = 0;
        let view = self.wb.view();
        self.stats_rows = view.row_count();
        self.stats_cols = view.col_count();
        self.status = format!(
            "{} · {} rows × {} cols",
            self.wb.active_name(),
            fmt_int(self.stats_rows),
            self.stats_cols
        );
        self.sync_formula_bar();
    }

    /// Add a fresh, empty in-RAM sheet and switch to it.
    ///
    /// New sheets are always `Sheet::new` — small and in RAM. Only a FILE gets
    /// the size check that decides mmap, and a blank sheet has no file, so
    /// there is nothing to be out-of-core about. Opening a 12 GB CSV into a
    /// sheet still takes the mmap path, per sheet.
    fn add_sheet(&mut self) {
        let name = self.wb.unique_sheet_name();
        let base = BaseData::Memory(Sheet::new(&name));
        match self.wb.add_sheet(&name, base) {
            Ok(id) => {
                self.switch_sheet(id);
                self.status = format!("Added sheet {name}");
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    fn delete_sheet(&mut self, id: ferrix_core::SheetId) {
        let name = self.wb.sheet_name(id).unwrap_or("").to_string();
        match self.wb.delete_sheet(id) {
            Ok(()) => {
                // Deleting may have changed which sheet is active.
                let state = self.wb.view_state();
                self.scroll = state.scroll;
                self.selection = state.selection;
                self.col_widths = Vec::new();
                let view = self.wb.view();
                self.stats_rows = view.row_count();
                self.stats_cols = view.col_count();
                self.status = format!("Deleted sheet {name}");
                self.sync_formula_bar();
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    fn commit_rename(&mut self) {
        let Some(id) = self.renaming.take() else {
            return;
        };
        let name = std::mem::take(&mut self.rename_buffer);
        match self.wb.rename_sheet(id, &name) {
            Ok(()) => {
                self.status = format!("Renamed sheet to {}", name.trim());
                self.sync_formula_bar();
            }
            // Refused (blank or duplicate): say why and keep the old name.
            Err(e) => self.status = format!("Rename refused — {e}"),
        }
    }

    /// The sheet tab strip along the bottom of the window.
    fn show_sheet_tabs(&mut self, ctx: &egui::Context) {
        let th = self.theme;
        // Actions are collected here and applied after the UI closure, which
        // borrows `self` immutably while every one of them needs `&mut self`.
        let mut switch_to = None;
        let mut delete = None;
        let mut finish_rename: Option<bool> = None;
        let mut add = false;
        let mut reorder: Option<(ferrix_core::SheetId, usize)> = None;

        let tabs: Vec<(ferrix_core::SheetId, String)> = self
            .wb
            .sheet_names()
            .into_iter()
            .map(|(id, n)| (id, n.to_string()))
            .collect();
        let active = self.wb.active_sheet();
        let can_delete = tabs.len() > 1;
        let pointer_down = ctx.input(|i| i.pointer.any_down());

        egui::TopBottomPanel::bottom("sheet_tabs")
            .frame(
                egui::Frame::none()
                    .fill(th.panel)
                    .inner_margin(egui::Margin::symmetric(6.0, 3.0)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (idx, (id, name)) in tabs.iter().enumerate() {
                                if self.renaming == Some(*id) {
                                    let resp = ui.add(
                                        egui::TextEdit::singleline(&mut self.rename_buffer)
                                            .desired_width(110.0)
                                            .font(egui::TextStyle::Body),
                                    );
                                    if self.rename_focus_pending {
                                        resp.request_focus();
                                        self.rename_focus_pending = false;
                                    } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                                        finish_rename = Some(false);
                                    } else if resp.lost_focus()
                                        || ui.input(|i| i.key_pressed(Key::Enter))
                                    {
                                        finish_rename = Some(true);
                                    }
                                    continue;
                                }

                                let resp = ui
                                    .selectable_label(*id == active, name.as_str())
                                    .on_hover_text(
                                        "Click to switch · double-click to rename · \
                                         drag to reorder · right-click for more",
                                    );
                                if resp.clicked() {
                                    switch_to = Some(*id);
                                }
                                if resp.double_clicked() {
                                    self.renaming = Some(*id);
                                    self.rename_buffer = name.clone();
                                    self.rename_focus_pending = true;
                                }
                                // Drag to reorder: whichever tab the pointer is
                                // over while a drag is live becomes the target.
                                if resp.drag_started() {
                                    self.dragging_tab = Some(*id);
                                }
                                if let Some(dragged) = self.dragging_tab {
                                    if dragged != *id && resp.hovered() && pointer_down {
                                        reorder = Some((dragged, idx));
                                    }
                                }
                                resp.context_menu(|ui| {
                                    if ui.button("Rename…").clicked() {
                                        self.renaming = Some(*id);
                                        self.rename_buffer = name.clone();
                                        self.rename_focus_pending = true;
                                        ui.close_menu();
                                    }
                                    if ui
                                        .add_enabled(can_delete, egui::Button::new("Delete"))
                                        .on_disabled_hover_text(
                                            "A workbook must keep at least one sheet",
                                        )
                                        .clicked()
                                    {
                                        delete = Some(*id);
                                        ui.close_menu();
                                    }
                                });
                            }
                            if ui.button("+").on_hover_text("Add a sheet").clicked() {
                                add = true;
                            }
                        });
                    });
            });

        if !pointer_down {
            self.dragging_tab = None;
        }
        match finish_rename {
            Some(true) => self.commit_rename(),
            Some(false) => {
                // Cancelled: keep the existing name.
                self.renaming = None;
                self.rename_buffer.clear();
            }
            None => {}
        }
        if let Some((id, to)) = reorder {
            let _ = self.wb.reorder_sheet(id, to);
        }
        if let Some(id) = switch_to {
            self.switch_sheet(id);
        }
        if let Some(id) = delete {
            self.delete_sheet(id);
        }
        if add {
            self.add_sheet();
        }
    }

    /// Re-run the search for the current input.
    ///
    /// Cheap enough to call on every keystroke even at 200M rows, because the
    /// engine matches the needle against the string arena first and only then
    /// scans columns as integers.
    fn run_search(&mut self) {
        // Derived, not fixed: every hit costs a `CellRef` in the results
        // vector plus a slot in the row-filter mapping built from them, and a
        // query matching every cell of a 200M-row sheet would otherwise
        // allocate without bound. The old flat 100k was safe on any machine
        // and needlessly small on most.
        let limit = ferrix_core::Budget::sample()
            .max_units_usize(ferrix_core::budget::cost::SEARCH_HIT)
            // Results are also scanned linearly to position the cursor, so
            // keep the cap sane on a huge machine rather than letting it grow
            // until the search itself becomes the slow part.
            .min(5_000_000);
        let Some(query) = ferrix_core::Query::new(
            self.search_input.trim(),
            self.search_case_sensitive,
            self.search_whole_cell,
        ) else {
            self.search_results = ferrix_core::SearchResults::default();
            self.search_index = 0;
            self.rebuild_row_filter();
            return;
        };
        let view = self.wb.view();
        self.search_results = view.search(&query, limit);
        // Resume from wherever the cursor is rather than jumping to the top.
        self.search_index = self.search_results.index_at_or_after(self.selection.cursor);
        // The mapping is derived ONCE here, not per frame.
        self.rebuild_row_filter();
        self.jump_to_current_match();
    }

    /// Rebuild the filter mapping from the current results.
    ///
    /// Called when a search runs and when the toggle flips — never from the
    /// paint path, which only ever borrows the finished mapping.
    fn rebuild_row_filter(&mut self) {
        self.row_filter = if self.search_filter_mode && self.search_open {
            Some(ferrix_core::RowFilter::from_results(&self.search_results))
        } else {
            None
        };
    }

    /// Turn filter mode on or off, keeping the viewport and selection anchored
    /// to the row the user was looking at.
    fn toggle_filter_mode(&mut self) {
        self.search_filter_mode = !self.search_filter_mode;
        // An in-progress edit belongs to a row whose screen position is about
        // to change underneath it; commit rather than let it land elsewhere.
        if self.editing.is_some() {
            self.commit_edit();
        }
        self.rebuild_row_filter();

        // Pull everything needed out of the mapping first: the borrow must not
        // still be live when the selection/status are updated.
        let plan = self.row_filter.as_ref().map(|f| {
            let row = if f.is_empty() {
                None
            } else {
                let want = f
                    .visible_at_or_after(self.selection.cursor.row)
                    .min(f.len() - 1);
                f.underlying(want)
            };
            (row, f.len(), f.truncated())
        });

        match plan {
            Some((None, _, _)) => {
                self.scroll.row_offset = 0.0;
                self.status = "Filter mode: no matching rows to show".into();
            }
            Some((Some(row), kept, truncated)) => {
                // Park on the first kept row at or after the cursor, so
                // switching the filter on does not silently strand the
                // selection on a row that is no longer rendered.
                self.selection
                    .move_to(CellRef::new(row, self.selection.cursor.col));
                self.sync_formula_bar();
                self.status = format!(
                    "Filter mode on — {} of {} row{} shown{}",
                    fmt_int(kept),
                    fmt_int(self.wb.view().row_count()),
                    if kept == 1 { "" } else { "s" },
                    if truncated {
                        " (capped — not all matches)"
                    } else {
                        ""
                    }
                );
                self.scroll_to_selection();
            }
            None => {
                self.status = "Filter mode off".into();
                self.scroll_to_selection();
            }
        }
    }

    /// Scroll offset space is VISIBLE rows, so an underlying row has to be
    /// mapped before it can be compared against `scroll.row_offset`.
    fn visible_row_of(&self, row: u32) -> Option<f64> {
        match &self.row_filter {
            Some(f) => f.visible_of(row).map(|v| v as f64),
            None => Some(row as f64),
        }
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
        // Closing search must restore the full sheet: leaving rows hidden with
        // no visible search bar would look like data loss.
        self.row_filter = None;
        // Re-anchor the viewport in unfiltered row space.
        self.scroll_to_selection();
        self.focus = Focus::Grid;
    }

    /// Centre the viewport on the selection — used when jumping to a match, so
    /// the hit lands mid-screen rather than scraping the edge.
    fn center_on_selection(&mut self) {
        let visible = (self.last_viewport_h / crate::grid::ROW_HEIGHT) as f64;
        let Some(row) = self.visible_row_of(self.selection.cursor.row) else {
            return;
        };
        let target = row - visible / 2.0;
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
                // Evaluate through the WORKBOOK, not just this sheet, so the
                // preview honours `Sheet2!A1` exactly as a committed formula
                // would.
                let source = crate::workbook::WorkbookSource::new(&self.wb, self.wb.active_sheet());
                let v = eval_view(&expr, &source);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let shown = match v {
                    Value::Number(n) => ferrix_core::format_number(n),
                    Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
                    Value::Text(id) => ferrix_formula::CellSource::resolve(&source, id).to_string(),
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
        let (ctrl_b, ctrl_i, ctrl_u) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::B),
                i.modifiers.command && i.key_pressed(Key::I),
                i.modifiers.command && i.key_pressed(Key::U),
            )
        });
        // Type shortcuts are ignored while editing a cell, where the same keys
        // belong to the text field.
        if self.editing.is_none() {
            if ctrl_b {
                let on = !self.selection_typography().resolved(12.5).bold;
                self.apply_typography(move |t| t.bold = Some(on));
            }
            if ctrl_i {
                let on = !self.selection_typography().resolved(12.5).italic;
                self.apply_typography(move |t| t.italic = Some(on));
            }
            if ctrl_u {
                let on = !self.selection_typography().resolved(12.5).underline;
                self.apply_typography(move |t| t.underline = Some(on));
            }
        }

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
            let _ = self.save_edits();
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
        // eframe's Frame is only a handle for viewport commands, which this
        // app does not use here. Keeping the real work in `frame` lets the
        // test harness drive the SAME code path without an eframe::Frame,
        // which cannot be constructed outside eframe.
        self.frame(ctx);
    }
}

impl FerrixApp {
    /// One frame of the app. This is the real update path; `eframe::App`
    /// delegates to it, and the headless harness calls it directly.
    pub fn frame(&mut self, ctx: &egui::Context) {
        self.poll_load();
        self.poll_export();
        // Refresh the memory reading at most once a second. Sampling is a
        // syscall; doing it per frame at 60fps would be measurable for no
        // benefit, and the number does not move meaningfully faster than that.
        self.budget = ferrix_core::budget::cached();
        if self.loading || self.exporting {
            ctx.request_repaint();
        }
        let frame_start = std::time::Instant::now();

        // Follow the OS preference until the user picks a theme themselves.
        // egui only learns the system theme once a frame has run, so this
        // cannot happen in `new`; and because it keeps running until the user
        // chooses, flipping the OS theme mid-session follows along.
        if !self.theme_chosen {
            let os = match ctx.system_theme() {
                Some(egui::Theme::Light) => ThemeMode::Light,
                Some(egui::Theme::Dark) => ThemeMode::Dark,
                // The platform does not expose one — stay on the default.
                None => self.theme.mode,
            };
            if os != self.theme.mode {
                self.theme = Theme::of(os);
            }
        }
        // Push the palette into egui's own widget styling every frame: it is
        // idempotent, and it means buttons, windows, scrollbars and text edits
        // switch with the grid rather than a frame later.
        self.theme.apply(ctx);
        let th = self.theme;

        // --- unsaved-changes guard on window close ---
        //
        // egui 0.29 reports a close request as viewport state; cancelling it
        // and showing a modal is the only way to stop the window vanishing
        // with unsaved edits in it. `allow_close` lets the second, deliberate
        // close request through instead of looping.
        if ctx.input(|i| i.viewport().close_requested()) && self.wb.is_dirty() && !self.allow_close
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_prompt = true;
        }

        if self.close_prompt {
            self.show_close_prompt(ctx);
        }
        self.show_chart_window(ctx);
        {}

        self.handle_keys(ctx);

        // --- toolbar ---
        //
        // The toggles are recorded and acted on after the panel closes: the
        // closure holds `&mut self` fields, so mutating the theme in place
        // here would conflict with the `th` the same frame is painting with.
        let mut toggle_theme = false;
        let mut toggle_empty = false;
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::none().fill(th.panel).inner_margin(8.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FERRIX").color(th.accent).strong().size(15.0));
                    ui.add_space(12.0);
                    if ui.button("Open CSV…").clicked() {
                        self.open_dialog();
                    }
                    if ui
                        .button("⬈ Export CSV…")
                        .on_hover_text("Write this sheet, including edits, to a CSV file")
                        .clicked()
                    {
                        self.export_dialog();
                    }
                    if ui
                        .button("📈 Chart…")
                        .on_hover_text("Chart the selected range")
                        .clicked()
                    {
                        self.open_chart();
                    }
                    ui.separator();
                    if ui
                        .button("⬓ Merge")
                        .on_hover_text("Merge the selection, or unmerge it if already merged")
                        .clicked()
                    {
                        self.toggle_merge();
                    }
                    self.type_controls(ui, th);
                    ui.separator();
                    if ui
                        .button("Open xlsx…")
                        .on_hover_text(
                            "Open a workbook, importing any Excel Tables with their \
                             validation, formatting, and filters",
                        )
                        .clicked()
                    {
                        self.open_xlsx_dialog();
                    }
                    if ui
                        .add_enabled(!self.tables.is_empty(), egui::Button::new("⬈ Export xlsx…"))
                        .on_hover_text(
                            "Write this sheet and its table as a real Excel Table, with \
                             dataValidation, conditionalFormatting, and autoFilter parts",
                        )
                        .clicked()
                    {
                        self.export_xlsx_dialog();
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
                        let _ = self.save_edits();
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
                    ui.add_space(4.0);
                    // --- theme toggle (issue #19) ---
                    if ui
                        .button(self.theme.mode.toggle_label())
                        .on_hover_text("Switch between light and dark. Remembered between runs.")
                        .clicked()
                    {
                        toggle_theme = true;
                    }
                    // --- empty rows toggle (issue #20) ---
                    if ui
                        .selectable_label(self.show_empty_rows, "⬓ Empty rows")
                        .on_hover_text(
                            "Show empty rows past the end of the sheet so there is \
                             somewhere to type. They are not data: exports, SUM and \
                             the row count ignore them until you type in one.",
                        )
                        .clicked()
                    {
                        toggle_empty = true;
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
                            ui.label(RichText::new("opening…").color(th.text_dim));
                        }
                        // Every operation over a second needs a way out that
                        // is not killing the process.
                        if ui.button("✖ Cancel").clicked() {
                            self.cancel_load();
                        }
                    }

                    if self.exporting {
                        ui.add(egui::Spinner::new().size(14.0));
                        if self.export_progress.total > 0 {
                            let frac = self.export_progress.done as f32
                                / self.export_progress.total as f32;
                            ui.add(egui::ProgressBar::new(frac).desired_width(180.0).text(
                                format!(
                                    "export {:.0}%  ({}/{} rows)",
                                    frac * 100.0,
                                    fmt_int(self.export_progress.done as usize),
                                    fmt_int(self.export_progress.total as usize)
                                ),
                            ));
                        } else {
                            ui.label(RichText::new("exporting…").color(self.theme.text_dim));
                        }
                        if ui.button("✖ Cancel export").clicked() {
                            self.cancel_export();
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
                            ui.label(RichText::new(label).color(th.text_dim).size(12.0));
                        }
                        // Say what the machine actually has and how much of it
                        // we are willing to use. A cap the user cannot see is
                        // indistinguishable from a bug.
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                self.budget.describe(),
                                ferrix_io::pool::describe()
                            ))
                            .color(self.theme.text_dim)
                            .size(12.0),
                        );
                    });
                });
            });

        if toggle_theme {
            self.set_theme(self.theme.mode.toggled());
        }
        if toggle_empty {
            self.set_show_empty_rows(!self.show_empty_rows);
        }

        // --- formula bar ---
        egui::TopBottomPanel::top("formula_bar")
            .frame(
                egui::Frame::none()
                    .fill(th.header_bg)
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(self.selection.label())
                            .color(th.accent)
                            .monospace()
                            .size(13.0),
                    );
                    ui.separator();
                    ui.label(RichText::new("fx").color(th.text_dim).italics());

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
                            th.error
                        } else {
                            th.number
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
                        .fill(th.header_bg)
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

                        // Match options. Toggling either re-runs the search
                        // immediately so the effect is visible.
                        if ui
                            .selectable_label(self.search_case_sensitive, "Aa")
                            .on_hover_text("Match case")
                            .clicked()
                        {
                            self.search_case_sensitive = !self.search_case_sensitive;
                            self.run_search();
                        }
                        if ui
                            .selectable_label(self.search_whole_cell, "[ab]")
                            .on_hover_text("Match entire cell contents")
                            .clicked()
                        {
                            self.search_whole_cell = !self.search_whole_cell;
                            self.run_search();
                        }
                        // Filter mode: hide every row without a match.
                        if ui
                            .selectable_label(self.search_filter_mode, "⬍ Filter")
                            .on_hover_text(
                                "Filter rows: show only rows containing a match. \
                                 Row numbers stay original.",
                            )
                            .clicked()
                        {
                            self.toggle_filter_mode();
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
                                    .color(th.text_dim)
                                    .size(11.5),
                            );
                        } else if r.total == 0 {
                            ui.label(RichText::new("no matches").color(th.error).size(11.5));
                        } else {
                            let shown = format!(
                                "{} of {}{}",
                                self.search_index + 1,
                                fmt_int(r.total),
                                if r.truncated { " (capped)" } else { "" }
                            );
                            ui.label(RichText::new(shown).color(th.text).size(11.5));
                            // Filter mode changes what "capped" costs the
                            // user: unfiltered they can still step past the
                            // cap with F3, but a filtered view LOOKS complete
                            // — you scroll to the bottom and it ends. Say so
                            // loudly rather than let them conclude wrongly.
                            if let Some(f) = &self.row_filter {
                                ui.label(
                                    RichText::new(format!(
                                        "· showing {} row{}",
                                        fmt_int(f.len()),
                                        if f.len() == 1 { "" } else { "s" }
                                    ))
                                    .color(th.text_dim)
                                    .size(11.0),
                                );
                                if f.truncated() {
                                    ui.label(
                                        RichText::new(format!(
                                            "⚠ INCOMPLETE — first {} of {} matches only; \
                                             more matching rows are NOT shown",
                                            fmt_int(self.search_results.matches.len()),
                                            fmt_int(f.total_matches())
                                        ))
                                        .color(th.error)
                                        .size(11.5)
                                        .strong(),
                                    )
                                    .on_hover_text(
                                        "Search stops collecting at 100,000 matches, so this \
                                         filtered view is a prefix of the matching rows. \
                                         Narrow the search to see a complete set.",
                                    );
                                }
                            }
                            // Surfacing why it was fast: N distinct strings
                            // matched, not N cells compared.
                            ui.label(
                                RichText::new(format!(
                                    "· {} ms · {} distinct string{} matched",
                                    r.millis,
                                    r.matched_strings,
                                    if r.matched_strings == 1 { "" } else { "s" }
                                ))
                                .color(th.text_dim)
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
                    .fill(th.header_bg)
                    .inner_margin(egui::Margin::symmetric(8.0, 4.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&self.status).color(th.text_dim).size(11.5));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let fps = if self.frame_ms > 0.0 {
                            1000.0 / self.frame_ms
                        } else {
                            0.0
                        };
                        let color = if fps >= 55.0 {
                            th.number
                        } else if fps >= 30.0 {
                            th.text_dim
                        } else {
                            th.error
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
                        // Invalid-cell badge. The count is honest even when
                        // the report's list was capped, so a table with a
                        // million bad rows says so instead of saying "1000".
                        if self.table_report.total > 0 {
                            ui.label(
                                RichText::new(format!("⚠ {} invalid", self.table_report.total))
                                    .color(th.invalid_flag)
                                    .size(11.5)
                                    .monospace(),
                            );
                        }
                        if let Some(m) = &self.table_mask {
                            ui.label(
                                RichText::new(format!(
                                    "filtered {} / {}",
                                    m.visible_rows(),
                                    m.total_rows()
                                ))
                                .color(th.accent)
                                .size(11.5)
                                .monospace(),
                            );
                        }
                    });
                });
            });

        // Column widths belong to the sheet in front of us. Switching tabs
        // clears them; recompute lazily here rather than on the switch, so a
        // sheet's widths are sized from its own data.
        if self.col_widths.is_empty() {
            if let crate::sheet_view::BaseData::Memory(s) = &*self.wb.base {
                self.col_widths = compute_col_widths_mem(s);
            }
        }

        // --- sheet tabs (below the status bar, like every spreadsheet) ---
        self.show_sheet_tabs(ctx);

        // --- grid ---
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(th.bg))
            .show(ctx, |ui| {
                if self.wb.view().row_count() == 0 {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("Open a CSV to get started")
                                .color(th.text_dim)
                                .size(16.0),
                        );
                    });
                    return;
                }

                let outer = ui.available_rect_before_wrap();
                self.last_viewport_h = outer.height() - crate::grid::HEADER_HEIGHT;

                // Filter mode with nothing to show: say so instead of painting
                // an empty grid that looks like an empty file.
                if self.row_filter.as_ref().is_some_and(|f| f.is_empty()) {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("No rows match — filter mode is hiding every row")
                                .color(th.text_dim)
                                .size(15.0),
                        );
                    });
                    return;
                }

                let resp = {
                    let view = self.wb.view();
                    // Table decoration is prepared once per frame for the
                    // visible rows only, so its cost is independent of how
                    // many rows the table covers.
                    let decor = self.tables.first().map(|t| {
                        let first = self.scroll.row_offset.floor().max(0.0) as u32;
                        let count =
                            (self.last_viewport_h / crate::grid::ROW_HEIGHT).ceil() as u32 + 1;
                        crate::table_view::TableDecor::prepare(
                            t,
                            self.table_mask.as_ref(),
                            &self.table_uniques,
                            &view,
                            first..first.saturating_add(count),
                        )
                    });
                    Grid {
                        view: &view,
                        selection: Some(self.selection),
                        col_widths: &self.col_widths,
                        scroll: &mut self.scroll,
                        editing: self.editing,
                        matches: &self.search_results.matches,
                        filling: self.fill_source.is_some(),
                        header_dragging: self.header_drag,
                        filter: self.row_filter.as_ref(),
                        table: decor.as_ref(),
                        current_match: if self.search_open {
                            self.search_results.wrapped(self.search_index)
                        } else {
                            None
                        },
                        theme: th,
                        format: Some(&self.wb.format),
                        merges: Some(&self.wb.merges),
                        pad_rows: if self.show_empty_rows {
                            crate::grid::EMPTY_ROW_PADDING
                        } else {
                            0
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
                // --- header reorder ---
                //
                // Press starts the drag and selects the whole column, so the
                // user sees what they grabbed. Release commits the move.
                if let Some(c) = resp.header_press {
                    self.header_drag = Some(c);
                    let last = self.stats_rows.saturating_sub(1) as u32;
                    self.selection =
                        Selection::new(CellRef::new(0, c as u32), CellRef::new(last, c as u32));
                    self.status = format!("Column {} selected", ferrix_core::column_name(c as u32));
                }
                if let (Some(src), Some(dst)) = (self.header_drag, resp.header_drag_to) {
                    if src != dst {
                        self.status = format!(
                            "Move {} → before {}",
                            ferrix_core::column_name(src as u32),
                            ferrix_core::column_name(dst as u32)
                        );
                    }
                }
                if resp.header_released {
                    if let (Some(src), Some(dst)) = (self.header_drag, resp.header_drag_to) {
                        if src != dst {
                            // `to` is an insertion point in the ORIGINAL
                            // indexing: dropping onto a column to the right
                            // means landing after it, hence dst + 1.
                            let to = if dst > src { dst + 1 } else { dst };
                            match self.move_columns(src as u64, 1, to as u64) {
                                Ok(()) => {
                                    self.status = format!(
                                        "Moved column {} to position {}",
                                        ferrix_core::column_name(src as u32),
                                        ferrix_core::column_name(dst as u32)
                                    );
                                }
                                Err(e) => self.status = format!("Move failed: {e}"),
                            }
                        }
                    }
                    self.header_drag = None;
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
                        let limit = self.max_overlay_cells();
                        match self.wb.fill_range(src, tgt, limit) {
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
                    // The editor must be placeable over a padding row too, and
                    // a padding row is in no filter's mapping — so the pad
                    // space is handed over alongside the filter.
                    let pad = self.pad_space();
                    if let Some(rect) = Grid::cell_screen_rect(
                        cell,
                        outer,
                        &self.scroll,
                        &self.col_widths,
                        self.row_filter.as_ref(),
                        pad,
                    ) {
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

        // Keep the active sheet's remembered position current, so switching
        // away at any moment restores exactly this view on the way back.
        self.stash_view_state();

        let ms = frame_start.elapsed().as_secs_f64() as f32 * 1000.0;
        self.frame_ms = if self.frame_ms == 0.0 {
            ms
        } else {
            self.frame_ms * 0.9 + ms * 0.1
        };
    }
}

/// Export adapter over the live base+overlay view.
///
/// Exporting must write what the user *sees* — base data with edits applied —
/// not the untouched base. `SheetView` already composes those, so the exporter
/// reads through it rather than reimplementing the merge.
impl ferrix_io::export::ExportSource for crate::sheet_view::SheetView<'_> {
    fn row_count(&self) -> usize {
        crate::sheet_view::SheetView::row_count(self)
    }
    fn col_count(&self) -> usize {
        crate::sheet_view::SheetView::col_count(self)
    }
    fn display(&self, cell: CellRef) -> String {
        crate::sheet_view::SheetView::display(self, cell)
    }
    fn header(&self, col: usize) -> String {
        crate::sheet_view::SheetView::header_or_letter(self, col)
    }
}

/// Assemble a `Workbook` from what a load produced.
///
/// Split out of `poll_load` so the multi-sheet wiring — tab order, per-sheet
/// formula overlays, and the cross-sheet graph build — is testable without an
/// egui context.
fn build_workbook(
    base: BaseData,
    sheet_name: String,
    first_formulas: Option<ferrix_core::EditOverlay>,
    restored: Option<ferrix_core::EditOverlay>,
    extras: Vec<(String, BaseData, ferrix_core::EditOverlay)>,
) -> Workbook {
    let had_formulas = first_formulas.as_ref().is_some_and(|o| !o.is_empty());
    let restored_any = restored.is_some();
    // The first sheet's overlay is whichever we have: a restored sidecar
    // (CSV path) or the xlsx's own formulas.
    let first_overlay = restored.or(first_formulas);
    let mut wb = Workbook::with_name(base, &sheet_name);
    if let Some(ov) = first_overlay {
        wb = wb.with_overlay(ov);
    }
    // Add the remaining sheets in source order. `add_sheet` inserts after the
    // ACTIVE tab, so activating each one as we go keeps the workbook's tab
    // order matching the file's.
    let mut added = 0usize;
    for (name, base, formulas) in extras {
        // A duplicate name cannot come from a valid xlsx, but if one does,
        // fall back to a generated name rather than dropping the sheet's data.
        let name = if wb.sheet_id_by_name(&name).is_some() {
            wb.unique_sheet_name()
        } else {
            name
        };
        if let Ok(id) = wb.add_sheet(&name, base) {
            let _ = wb.activate(id);
            wb.adopt_overlay(formulas);
            added += 1;
        }
    }
    let _ = wb.activate_index(0);
    // Formula cells arrive with their source and a cached value computed
    // elsewhere; rebuild the graph and recompute so nothing drifts. This is
    // also what wires up cross-sheet references between the sheets just
    // loaded — until every sheet exists, `Sheet2!A1` has nothing to resolve to.
    if restored_any || had_formulas || added > 0 {
        wb.rebuild_graph_and_recalc();
    }
    wb
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
fn load_any<F, C>(path: &Path, mut progress: F, should_cancel: &mut C) -> LoadResult
where
    F: FnMut(u64, u64),
    C: FnMut() -> bool,
{
    // xlsx is inherently multi-sheet, and every sheet lands as its own
    // independently stored `BaseData`.
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xlsx"))
    {
        return load_xlsx(path);
    }

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Sheet1".to_string());

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
            sheet_name: stem,
            extra_sheets: Vec::new(),
            first_formulas: None,
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
        let stats =
            ferrix_io::convert_csv_cancellable(path, &cache, b',', true, &mut progress, || {
                should_cancel()
            })
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
        sheet_name: stem,
        extra_sheets: Vec::new(),
        first_formulas: None,
        edits_path,
        fingerprint,
        restored,
        edit_warning,
    })
}

/// Open an .xlsx workbook, populating EVERY sheet.
///
/// Each worksheet becomes its own in-RAM `BaseData` plus its own formula
/// overlay, so cross-sheet formulas resolve and each sheet's storage stays
/// independent. Sheets always fit in RAM here because xlsx itself caps out at
/// ~1M rows per sheet — the mmap path is for CSVs that dwarf that.
fn load_xlsx(path: &Path) -> LoadResult {
    let t = std::time::Instant::now();
    let imported = ferrix_io::import_xlsx_full(path).map_err(|e| e.to_string())?;
    let sheet_count = imported.len();
    let total_cells: usize = imported.iter().map(|s| s.stats.cells).sum();
    let kept: usize = imported.iter().map(|s| s.stats.formulas_kept).sum();
    let dropped: usize = imported.iter().map(|s| s.stats.formulas_dropped).sum();

    let mut it = imported.into_iter();
    let first = it
        .next()
        .ok_or_else(|| "workbook has no sheets".to_string())?;
    let widths = compute_col_widths_mem(&first.sheet);
    let rows = first.sheet.row_count();
    let cols = first.sheet.col_count();

    let extra_sheets: Vec<(String, BaseData, ferrix_core::EditOverlay)> = it
        .map(|s| (s.name, BaseData::Memory(s.sheet), s.formulas))
        .collect();

    let summary = format!(
        "Loaded {} sheet{} · {} cells · {} formula{} in {:.0} ms{}",
        sheet_count,
        if sheet_count == 1 { "" } else { "s" },
        fmt_int(total_cells),
        fmt_int(kept),
        if kept == 1 { "" } else { "s" },
        t.elapsed().as_secs_f64() * 1000.0,
        if dropped > 0 {
            format!(
                " · {} unsupported formula(s) kept as values",
                fmt_int(dropped)
            )
        } else {
            String::new()
        }
    );

    Ok(Loaded {
        rows,
        cols,
        col_widths: widths,
        summary,
        base: BaseData::Memory(first.sheet),
        sheet_name: first.name,
        extra_sheets,
        first_formulas: Some(first.formulas),
        // Sidecar edits are a CSV/mmap concept: an xlsx carries its own
        // formulas, and pairing a sidecar with a re-saved workbook would be a
        // silent mismatch waiting to happen.
        edits_path: None,
        fingerprint: None,
        restored: None,
        edit_warning: None,
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

    // --- multi-sheet loading (issue #15) ---

    /// A temp .xlsx that deletes itself, so a failed assert cannot leave a
    /// stray file behind for the next run to trip over.
    struct TempXlsx(PathBuf);

    impl TempXlsx {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "ferrix-ui-{tag}-{}-{:?}.xlsx",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_file(&p);
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempXlsx {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Write a real three-sheet workbook where the third sheet's formula
    /// reaches across to the first.
    fn write_three_sheet_xlsx(path: &Path) {
        use ferrix_core::{CellInput, EditOverlay};
        use ferrix_io::SheetExport;

        let mut alpha = Sheet::new("Alpha");
        for r in 0..4u32 {
            alpha.set(CellRef::new(r, 0), Value::Number((r + 1) as f64));
        }
        let mut beta = Sheet::new("Beta");
        beta.set_text(CellRef::new(0, 0), "beta text");
        beta.set(CellRef::new(1, 0), Value::Number(99.0));

        let mut gamma = Sheet::new("Gamma");
        // Excel's cached result for the formula below. Deliberately WRONG so
        // the test proves Ferrix recomputes rather than trusting the cache.
        gamma.set(CellRef::new(0, 0), Value::Number(-1.0));
        let mut gamma_fx = EditOverlay::new();
        gamma_fx.set(
            CellRef::new(0, 0),
            CellInput::Formula {
                src: "=SUM(Alpha!A1:A4)".into(),
                cached: Value::Number(-1.0),
            },
        );

        ferrix_io::export_workbook(
            path,
            &[
                SheetExport::new("Alpha", &alpha),
                SheetExport::new("Beta", &beta),
                SheetExport::new("Gamma", &gamma).with_formulas(&gamma_fx),
            ],
        )
        .expect("export fixture");
    }

    #[test]
    fn importing_a_multi_sheet_xlsx_populates_every_sheet() {
        let tmp = TempXlsx::new("multisheet");
        write_three_sheet_xlsx(tmp.path());

        let loaded = load_any(tmp.path(), |_, _| {}, &mut || false).expect("load");
        assert_eq!(loaded.sheet_name, "Alpha");
        assert_eq!(loaded.extra_sheets.len(), 2, "Beta and Gamma must load too");

        let wb = build_workbook(
            loaded.base,
            loaded.sheet_name,
            loaded.first_formulas,
            loaded.restored,
            loaded.extra_sheets,
        );

        // Every sheet is present, in the workbook's own order.
        let names: Vec<String> = wb
            .sheet_names()
            .into_iter()
            .map(|(_, n)| n.to_string())
            .collect();
        assert_eq!(names, vec!["Alpha", "Beta", "Gamma"]);
        // ...and the first tab is the one in front of the user.
        assert_eq!(wb.active_name(), "Alpha");

        let id_of = |n: &str| wb.sheet_id_by_name(n).expect("sheet");
        let get = |n: &str, r: u32, c: u32| {
            wb.sheet_view(id_of(n))
                .expect("view")
                .get(CellRef::new(r, c))
        };

        // Each sheet kept its OWN data, in its own storage.
        assert_eq!(get("Alpha", 0, 0), Value::Number(1.0));
        assert_eq!(get("Alpha", 3, 0), Value::Number(4.0));
        assert_eq!(get("Beta", 1, 0), Value::Number(99.0));
        assert_eq!(
            wb.sheet_view(id_of("Beta"))
                .unwrap()
                .display(CellRef::new(0, 0)),
            "beta text"
        );

        // The cross-sheet formula on Gamma was rebuilt and RECOMPUTED against
        // the real Alpha data, not left at the bogus cached -1.
        assert_eq!(
            get("Gamma", 0, 0),
            Value::Number(10.0),
            "Gamma!A1 = SUM(Alpha!A1:A4) must recompute across sheets on load"
        );
    }

    #[test]
    fn a_loaded_cross_sheet_formula_recalculates_on_edit() {
        let tmp = TempXlsx::new("recalc");
        write_three_sheet_xlsx(tmp.path());
        let loaded = load_any(tmp.path(), |_, _| {}, &mut || false).expect("load");
        let mut wb = build_workbook(
            loaded.base,
            loaded.sheet_name,
            loaded.first_formulas,
            loaded.restored,
            loaded.extra_sheets,
        );
        let gamma = wb.sheet_id_by_name("Gamma").unwrap();

        // Editing Alpha must drive the Gamma formula, proving the graph built
        // on load really does span sheets.
        wb.commit_edit(CellRef::new(0, 0), "11"); // Alpha!A1: 1 -> 11
        assert_eq!(
            wb.sheet_view(gamma).unwrap().get(CellRef::new(0, 0)),
            Value::Number(20.0),
            "10 - 1 + 11"
        );
    }

    #[test]
    fn a_single_sheet_csv_load_still_makes_a_one_sheet_workbook() {
        // Regression guard: the CSV path must not sprout phantom sheets.
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Number(5.0));
        let wb = build_workbook(BaseData::Memory(s), "data".into(), None, None, Vec::new());
        assert_eq!(wb.sheet_count(), 1);
        assert_eq!(wb.active_name(), "data");
        assert_eq!(wb.view().get(CellRef::new(0, 0)), Value::Number(5.0));
    }
}
