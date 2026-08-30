//! Application state and top-level layout.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use egui::{Align, Key, Layout, RichText};
use ferrix_core::{CellRef, Selection, Sheet, Suggestions, Value};
use ferrix_formula::eval_view;
use ferrix_io::{load_csv, CsvOptions};

use crate::grid::{Grid, ScrollState, DEFAULT_COL_WIDTH};
use crate::prefs::Prefs;
// A menu choice cannot be dispatched inside the panel closure: the closure
// holds `self` while it renders enabled/disabled state, so the choice is
// recorded and acted on after the panel ends. That used to be two enums,
// `FileAction` and `ViewAction`; issue #40 replaced both with a single
// `Option<CommandId>`, because every menu item is now a registry command and
// there is one dispatcher.
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
    /// Where cell comments for this dataset are saved.
    ///
    /// A SEPARATE file from the edits sidecar, and deliberately not guarded by
    /// the base fingerprint: "check this with finance" on B7 stays true when
    /// the CSV is regenerated with a million more rows, so refusing it on a
    /// stale base would throw away annotations every time the data refreshed.
    comments_path: Option<PathBuf>,
    /// Comments restored from that sidecar.
    comments: ferrix_core::CommentMap,
    /// The `.ferrix` cache backing this sheet, when there is one. Only the
    /// out-of-core path has one; an in-RAM CSV or an xlsx has nothing to
    /// compact, which is exactly why the menu item is disabled for them.
    cache_path: Option<PathBuf>,
    /// Edits restored from a sidecar, if one was present and current.
    restored: Option<ferrix_core::EditOverlay>,
    /// Set when a sidecar existed but was rejected, so the UI can warn instead
    /// of silently discarding the user's saved work.
    edit_warning: Option<String>,
    /// An autosave newer than the sidecar — work a crash would otherwise have
    /// lost. Detected during load (two `stat`s), offered to the user on
    /// arrival. Nothing is applied until they choose Recover.
    recovery: Option<ferrix_io::edits::RecoveryCandidate>,
    /// Defined names read from the source. Only xlsx carries any; CSV yields
    /// an empty table, which costs nothing.
    names: ferrix_formula::NameTable,
    /// Sheet protection read from the source, by workbook-order sheet index
    /// (issue #42). Empty for CSV and for any xlsx that has none.
    protection: Vec<(usize, ferrix_core::SheetProtection)>,
    /// Workbook-structure protection read from the source.
    wb_protection: ferrix_core::WorkbookProtection,
}

type LoadResult = Result<Loaded, String>;

/// What a finished compact hands back to the UI thread.
///
/// The stats are flattened rather than passing `CompactStats` through, because
/// the residual overlay has to come across too and the UI wants one message
/// per completion, not two.
struct CompactDone {
    rows: u64,
    cols: usize,
    edits_baked: usize,
    formulas_kept: usize,
    output_bytes: u64,
    millis: u128,
    peak_heap_bytes: usize,
    /// Edits that could NOT be baked into a columnar file — formulas, which
    /// keep their source. Empty in the ordinary all-literals case.
    residual: ferrix_core::EditOverlay,
    /// The sidecar that now exists, or `None` when it was retired outright.
    sidecar: Option<PathBuf>,
}

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

/// Widget ids for the two places a formula can be typed (issue #38).
///
/// Named constants because F4 and the caret read-back have to address the
/// SAME widget the editor was drawn with; two spellings of the same string
/// would silently read an empty caret and put F4 on the wrong reference.
/// Cap on cells the Circle Invalid Data pass will ring (issue #41).
///
/// A second belt on top of the viewport bound. The circles are already
/// computed over the painted rows only, so this can never bite in practice —
/// it exists so a future caller that widened the range cannot turn a 200M-row
/// sheet into a 200M-entry `Vec`.
pub const MAX_CIRCLED: usize = 4096;

const CELL_EDITOR_ID: &str = "cell_editor";
const FORMULA_BAR_ID: &str = "formula_bar_edit";

/// Which axis a page-break drag is moving, and where it started (#76).
///
/// A break line is grabbed at the row (horizontal break) or column (vertical
/// break) it currently sits before; the drag then moves that manual break to
/// wherever the pointer is released. `origin` is the row/col the break was at
/// when grabbed, so a drag that lands back on it is a no-op rather than a
/// duplicate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BreakDrag {
    axis: BreakAxis,
    origin: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BreakAxis {
    Row,
    Col,
}

/// How many DISJOINT selection ranges are kept (issue #17).
///
/// Ctrl+clicking headers accumulates ranges, and without a cap a user leaning
/// on Ctrl would grow the list without bound. 64 is far more scattered ranges
/// than anyone assembles by hand, each is 16 bytes, and the paint loop tests
/// membership against all of them per visible cell — so the bound is what keeps
/// that per-cell cost a small constant rather than something that grows with
/// the session. The cap is REPORTED when it bites, never silently enforced.
pub const MAX_DISJOINT_SELECTIONS: usize = 64;

/// A highlighted reference outline being dragged onto another cell.
#[derive(Clone, Copy, Debug)]
struct RefDrag {
    /// Which reference in the formula, by source order.
    span: usize,
    /// Cell the pointer was over when the outline was grabbed. The offset is
    /// measured from HERE rather than from the reference's own corner, so
    /// grabbing an outline anywhere inside it and dropping it one cell right
    /// moves the reference one cell right — not to wherever the pointer
    /// happened to land relative to the corner.
    from: CellRef,
}

/// A block move/copy the user asked for whose destination would overwrite
/// non-empty cells — parked here while the confirmation modal is up (#82).
///
/// Excel prompts before a drag-drop clobbers data ("There's already data
/// here. Do you want to replace it?"); this holds the exact gesture so the
/// answer can carry it out unchanged, or drop it on Cancel.
#[derive(Clone, Copy, Debug)]
struct PendingBlockMove {
    /// The selection being moved/copied, as it stood when the drop landed.
    sel: Selection,
    d_row: i64,
    d_col: i64,
    /// True for a Ctrl-drag copy, false for a plain move.
    copy: bool,
}

pub struct FerrixApp {
    wb: Workbook,
    stats_rows: usize,
    stats_cols: usize,
    col_widths: Vec<f32>,
    /// Active selection. `cursor` is the cell that typing lands in.
    selection: Selection,
    /// Additional DISJOINT ranges, from Ctrl+clicking row or column headers
    /// (issue #17).
    ///
    /// Bounded on purpose. Each entry is two corners, so a hundred disjoint
    /// full-column selections over a 200M-row sheet is a few kilobytes rather
    /// than a row list — but the list itself still has to stop somewhere, or a
    /// held-down Ctrl+click would grow it without limit. See
    /// [`MAX_DISJOINT_SELECTIONS`].
    extra_selections: Vec<Selection>,
    scroll: ScrollState,

    focus: Focus,
    editing: Option<CellRef>,
    edit_buffer: String,
    /// True on the frame an edit begins, so we can grab keyboard focus once.
    just_started_edit: bool,

    formula_input: String,
    formula_result: Option<String>,

    // ---- formula bar upgrades (issue #38) ----
    /// The cell's text as it stood the instant the edit began, kept so Escape
    /// can restore it EXACTLY.
    ///
    /// The edit buffer cannot answer this once the edit was seeded by typing:
    /// the first keystroke replaces the cell, so by the time Escape arrives
    /// the only copy of `=SUM(B1:B3)` left is this one.
    edit_pre_text: String,
    /// Caret position, in BYTES, inside whichever editor is live. Read back
    /// from egui's own `TextEditState` each frame, because F4 has to act on
    /// the reference the user is actually parked on rather than on the first
    /// one in the formula.
    edit_caret: usize,
    /// Caret to install on the next frame, after a rewrite moved it.
    pending_caret: Option<usize>,
    /// Formula bar height in text rows. 1 is the classic single-line bar;
    /// anything more makes it a real multi-line editor. Persisted.
    formula_bar_rows: usize,
    /// Height the formula bar panel actually occupied last frame — real
    /// layout output, so a test can tell the drag handle apart from a field
    /// that merely stores a number.
    last_formula_bar_h: f32,
    /// Sheets showing formula SOURCE instead of values (Ctrl+`). Per sheet,
    /// because it is a way of looking at one sheet, not a global mode.
    show_formulas: std::collections::HashSet<ferrix_core::SheetId>,
    /// Reference outlines painted over the grid on the last frame, recorded
    /// AT THE POINT OF PAINTING: `(the span's rect, its colour)`.
    ///
    /// A test reads this rather than the model, so an outline that is
    /// computed but never drawn reports as absent.
    ref_outlines: Vec<(egui::Rect, egui::Color32)>,
    /// An in-progress drag of one of those outlines.
    ref_drag: Option<RefDrag>,

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
    /// Replace panel state. It sits beside the search box (Ctrl+H) rather than
    /// in its own window, so find and replace share one query and one set of
    /// options — toggling "match case" must mean the same thing to both.
    replace_open: bool,
    replace_input: String,
    replace_focus_pending: bool,
    /// Regex mode applies to the search query itself, so Ctrl+F and Ctrl+H
    /// agree on what a match is.
    search_regex: bool,
    /// Reported when a regex fails to compile. Silently finding nothing would
    /// be indistinguishable from "no matches", which is the worst answer to a
    /// typo'd pattern.
    search_regex_error: Option<String>,
    /// Whether a replace reads displayed values or formula source text.
    replace_look_in: ferrix_core::LookIn,
    /// Cancellation for a running Replace All. Held across frames so the
    /// Cancel button can flip it while the pass is in flight.
    replace_cancel: ferrix_core::CancelToken,
    /// Live `(examined, applied)` from the pass in progress, so a long replace
    /// reports rather than freezing silently.
    replace_progress: Option<(usize, usize)>,
    /// Trip the cancel flag once this many cells have been applied.
    ///
    /// Exists so a test can stop a Replace All at a KNOWN point instead of
    /// racing a timer thread against it. Always `None` in the running app,
    /// where the Cancel button flips `replace_cancel` directly.
    replace_cancel_after_applied: Option<usize>,
    /// The visible-row -> underlying-row mapping backing filter mode.
    ///
    /// Rebuilt once per search (and once when the toggle flips), never per
    /// frame. `None` whenever filter mode is off.
    row_filter: Option<ferrix_core::RowFilter>,

    /// Columns the view is sorted by, in priority order. Empty means unsorted.
    ///
    /// This is the SPEC; `sort_order` below is the mapping derived from it.
    /// Keeping them apart is what lets the spec survive a search change while
    /// the mapping is rebuilt over the new candidate rows.
    sort_keys: Vec<ferrix_core::SortKey>,
    /// The visible-row -> underlying-row mapping a sort produces.
    ///
    /// Rebuilt only when the spec, the filters, or the data change — never per
    /// frame. `None` whenever nothing is sorted, so an unsorted sheet pays
    /// exactly nothing.
    sort_order: Option<ferrix_core::SortOrder>,

    /// Active subtotal grouping (issue #34). `None` means no subtotals, and
    /// dropping it is exactly what "Remove Subtotals" does — which is why
    /// removing them restores the exact original view: there is nothing else
    /// to undo.
    ///
    /// A VIEW TRANSFORM, like `sort_order`: it inserts no data, marks nothing
    /// dirty, and pushes no undo entry.
    subtotals: Option<ferrix_core::SubtotalPlan>,
    /// The grouping spec the plan was built from, kept so the plan can be
    /// rebuilt when a sort or filter changes the rows underneath it.
    subtotal_spec: Option<(u32, Vec<u32>, ferrix_core::SubtotalFn)>,

    /// Chart panel state: the built scene, its annotations, and the window.
    chart: crate::chart_panel::ChartPanel,

    /// Display column being dragged by its header, between press and release.
    header_drag: Option<usize>,
    /// Row/column sizes, hidden spans and outline groups for the active sheet
    /// (issue #29).
    sizing: ferrix_core::sizing::SheetSizing,
    /// Print area: the range printed instead of the whole used extent, 0-based
    /// inclusive. `None` prints everything. Set from the selection and cleared
    /// by the File menu (#37). Stored once per sheet, not per cell.
    print_area: Option<ferrix_core::TableRange>,
    /// Page Break Preview: when on, the grid draws a dashed line at every row
    /// and column where a printed page would break (#37). Read-only — it shows
    /// where the paginator splits; it does not let the user drag a break.
    show_page_breaks: bool,
    /// The sheet's page setup: paper, margins and MANUAL page breaks. Held here
    /// (not rebuilt per frame) so a break the user inserts in the preview or
    /// drags to a new row actually persists and reaches the PDF/HTML export —
    /// both the preview overlay and the print path read this one value. Manual
    /// breaks are stored as a short sorted Vec, never per row, so it stays cheap
    /// on a 200M-row sheet. Reset when a new file loads (per sheet).
    page_setup: ferrix_core::page::PageSetup,
    /// A page-break line being dragged (#76). Records which break and axis the
    /// press grabbed, so the release can move it from its old row/col to the one
    /// under the pointer. `None` when no drag is in flight.
    break_drag: Option<BreakDrag>,
    /// How many page-break lines the last frame painted, for tests and the
    /// status note. Reset each frame.
    last_page_break_lines: usize,
    /// Set each frame: true when the app is driving continuous repaints for an
    /// in-content drag, false otherwise (incl. an OS window move). Testable
    /// witness for the window-move-jitter guard (#84).
    dragging_content: bool,
    /// Column being resized by its border: (column, pointer x at press, width
    /// at press). OWNED HERE rather than read from egui's `is_dragging`, which
    /// is cleared on the release frame — the width would be lost exactly when
    /// the release needs it.
    col_resize: Option<(usize, f32, f32)>,
    /// Header the hide/unhide context menu is open on.
    header_menu: Option<(usize, egui::Pos2)>,
    /// Rows the last autofit actually inspected. The acceptance criterion is a
    /// BOUND, not a width, so the bound is measured rather than assumed.
    last_autofit_rows: usize,
    /// The folded hidden-row index, rebuilt whenever hiding changes and
    /// composed into `RowResolver` as ONE stage. `None` when nothing is
    /// hidden, which keeps the default resolve path free of lookups.
    hidden_rows: Option<ferrix_core::sizing::HiddenRows>,
    /// Columns hidden explicitly or by a collapsed column group.
    hidden_cols: std::collections::BTreeSet<u32>,
    /// Set when sizing changed and the sidecar has not been written yet.
    sizing_dirty: bool,
    /// The rich payload of the LAST copy made in this window (issue #30).
    ///
    /// eframe's clipboard is plain text only, so the number formats and
    /// styling a copy captured have nowhere to live on the system clipboard.
    /// Holding them here means a Ferrix -> Ferrix round trip is lossless
    /// within a session; a paste only uses it when the text clipboard still
    /// matches, so copying elsewhere and pasting back never resurrects it.
    clip_block: Option<ferrix_core::clipboard::ClipBlock>,
    /// The same copy rendered as an HTML `<table>`.
    ///
    /// This is the flavour that WOULD be published beside the text one if
    /// eframe could publish flavours. It is kept so the rendering is exercised
    /// by the real copy path rather than only by unit tests, and so the wiring
    /// is a one-line change if that ever becomes possible.
    clip_html: Option<String>,
    /// The Paste Special request being assembled in the dialog, and whether
    /// the dialog is open.
    paste_special: Option<ferrix_core::clipboard::PasteOptions>,
    /// Outline toggle buttons the grid painted last frame — real paint output
    /// a test can assert the gutter against.
    last_outline_buttons: usize,
    /// Where sizing is persisted, beside the base file.
    sizing_path: Option<PathBuf>,
    /// Where pivot bindings are persisted, beside the base file (issue #33
    /// Part B). Derived from `source_path` on open, the same way `sizing_path`
    /// is, because a pivot binding — like sizing — is workbook state that stays
    /// true when the base data is regenerated, so it is keyed to the file rather
    /// than carried through the edits sidecar's base fingerprint.
    pivots_path: Option<PathBuf>,
    /// Where each visible column header was painted last frame. Recorded from
    /// the grid's own response so a caller never has to guess at header
    /// pixels, which move with every bar that opens above the grid.
    header_hitboxes: Vec<(usize, egui::Pos2)>,
    /// Where each visible ROW header was painted last frame (issue #17).
    /// Same purpose as `header_hitboxes`, for the other axis.
    row_header_hitboxes: Vec<(u32, egui::Pos2)>,
    /// (screen row, underlying row) for every row the LAST FRAME painted,
    /// frozen band first. Read back by tests as the app's own account of what
    /// is on screen.
    last_painted_rows: Vec<(usize, u32)>,
    /// How many of `last_painted_rows` were in the frozen/split band.
    last_frozen_rows: usize,
    /// The grid's outer rect as of the last frame, so cell geometry can be
    /// asked for outside the paint closure.
    last_grid_rect: Option<egui::Rect>,

    /// Selection a fill drag started from, and the live target while dragging.
    fill_source: Option<Selection>,
    fill_target: Option<Selection>,
    /// Cell grabbed at the start of a block move-drag (#82), so the drop cell's
    /// offset from it gives the move delta. `Some` only while a move is in
    /// flight.
    move_origin: Option<CellRef>,
    /// A block move/copy whose destination would overwrite data, awaiting the
    /// user's answer to the confirmation modal (#82). `None` the rest of the
    /// time, which costs nothing.
    pending_block_move: Option<PendingBlockMove>,

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

    // --- compact (roadmap #7) ---
    //
    // Compact rewrites the `.ferrix` cache with the overlay baked in, so the
    // sidecar can be retired. It is minutes of work on a 10 GB file and it
    // rewrites the user's data, so it runs on a worker with a modal in front
    // of it: no editing while the file underneath is being replaced.
    /// The cache this sheet reads from, when it is out-of-core.
    cache_path: Option<PathBuf>,
    /// Column headers recovered from the source, re-applied to the new
    /// mapping. The cache stores data only, so losing these would blank the
    /// header row on compact.
    cache_headers: Vec<String>,
    compacting: bool,
    compact_rx: Option<Receiver<Result<CompactDone, String>>>,
    compact_progress_rx: Option<Receiver<Progress>>,
    compact_progress: Progress,
    compact_cancel: Option<ferrix_core::CancelToken>,
    compact_started: Option<std::time::Instant>,

    // --- autosave (roadmap #8) ---
    //
    // Edits live in a sidecar and undo history is cleared on save, so a crash
    // between saves loses everything typed since the last one with no undo
    // left to recover it. The timer below is the safety net.
    /// When the last autosave tick fired. `None` until the first frame after
    /// a load, so a freshly opened file does not immediately write.
    autosave_last: Option<std::time::Instant>,
    /// The overlay revision captured at the last successful autosave. A tick
    /// whose revision matches this writes nothing at all — the "no change,
    /// no write" rule, decided in O(1) rather than by comparing overlays.
    autosave_revision: Option<u64>,
    /// A running background autosave, if any. The write happens on a worker
    /// thread against a cloned overlay so a large edit set never stalls a
    /// frame; at most one is in flight, since a second would race the first
    /// onto the same path.
    autosave_rx: Option<Receiver<Result<(u64, u64), String>>>,
    /// Recovery offer from a previous session's autosave, awaiting a choice.
    recovery: Option<ferrix_io::edits::RecoveryCandidate>,
    /// Set once the user has resolved the recovery prompt, so a redraw does
    /// not re-offer edits they already accepted or discarded.
    recovery_resolved: bool,

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

    /// Where cell comments are persisted, when the source has a sidecar.
    comments_path: Option<PathBuf>,
    /// Cell whose comment is being written or edited, plus the buffer.
    ///
    /// `None` means no editor is open. Held here rather than in the grid so
    /// the dialog survives the frame the grid is repainted on.
    comment_editing: Option<CellRef>,
    comment_buffer: String,
    comment_author_buffer: String,
    /// True on the frame the comment editor opens, so it takes focus once.
    comment_focus_pending: bool,
    /// Cell whose context menu is open, if any. Captured on right-press so the
    /// menu acts on the cell the user aimed at, not on wherever the selection
    /// happens to be.
    context_cell: Option<CellRef>,
    /// Screen point the context menu is anchored at.
    context_pos: egui::Pos2,

    /// Comment markers painted by the last frame.
    last_comment_markers: usize,

    /// Border edges, rotated texts and wrapped texts the grid painted last
    /// frame (issue #28). Real paint output, recorded from the grid's own
    /// response, so a test asserting "the border is actually drawn" is reading
    /// the screen rather than the format store.
    last_border_segments: usize,
    last_rotated_texts: usize,
    last_wrapped_texts: usize,
    /// Subtotal rows and aggregate texts the last frame painted (issue #34).
    last_subtotal_rows: usize,
    last_subtotal_texts: usize,
    /// Sparkline primitives, and covered-but-blank cells, the grid painted
    /// last frame (issue #36). Same discipline: the count of the SPECIFIC
    /// shape kind this feature emits, not a slice of the frame total.
    last_sparkline_shapes: usize,
    last_sparkline_blanks: usize,

    /// Active trace-precedents/dependents session (roadmap #39), if any.
    /// `None` means "Remove Arrows" was pressed or nothing has been traced.
    trace: Option<crate::trace::TraceState>,
    /// Arrows actually painted by the last frame, for the "showing N of M"
    /// status note and for tests asserting on real paint output rather than
    /// on the model.
    last_trace_arrows: usize,
    /// Total arrows the current trace level would draw before the cap —
    /// what the "showing N of M" note reports alongside `last_trace_arrows`.
    last_trace_total: usize,

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
    /// Frozen / split leading band for the active sheet.
    panes: crate::grid::Panes,
    /// Zoom for the active sheet, 0.25..=4.0. Persisted per sheet name.
    zoom: f32,

    /// Name Box buffer: what the user is typing above the row headers.
    ///
    /// `None` whenever the box is not being edited, which is what lets it
    /// display the live selection (a name, or the A1 label) rather than a
    /// stale string. It only becomes `Some` when the user starts typing.
    name_box_edit: Option<String>,
    /// Name Manager window state.
    names_open: bool,
    /// The name currently being edited in the manager, and the buffers it is
    /// being edited into. Held as (original identifier, scope) so an edit that
    /// changes the identifier still knows which entry it started from.
    name_edit: Option<(String, ferrix_formula::NameScope)>,
    name_edit_ident: String,
    name_edit_target: String,
    /// Last error from a name operation, shown in the manager. Kept separate
    /// from `status` so it stays visible while the modal is open.
    name_error: Option<String>,
    /// Whether a new name typed in the Name Box is scoped to the active sheet.
    name_box_sheet_scope: bool,

    /// The conditional-formatting editor, when it is open (roadmap #11).
    ///
    /// `None` is the overwhelmingly common case and costs nothing: no dialog,
    /// no preview clone, and the grid paints straight from `wb.format`.
    cond: Option<crate::cond_format::CondFormatState>,
    /// Last lossy-export warning the editor produced, mirrored into the status
    /// bar so it survives the dialog closing.
    cond_warning: Option<String>,

    // ---- data validation and autocomplete (issue #41) ----
    /// The validation editor, when it is open. `None` costs nothing.
    validation: Option<crate::validation_panel::ValidationState>,
    /// The in-cell suggestion popup's state.
    ///
    /// Holds at most `MAX_SUGGESTIONS` strings, so its footprint has a hard
    /// ceiling regardless of the column behind it.
    autocomplete: crate::validation_panel::AutocompleteState,
    /// Whether autocomplete is offering suggestions at all. A toggle, because
    /// it is the kind of help that some people want off.
    autocomplete_on: bool,
    /// Where the in-cell dropdown arrow was painted last frame, if anywhere.
    dropdown_button: Option<(CellRef, egui::Rect)>,
    /// Cells the Circle Invalid Data pass ringed last frame.
    ///
    /// Recomputed per frame from the VIEWPORT, so it is bounded by the screen
    /// and never by the sheet — a 200M-row sheet with every row invalid puts
    /// at most a screenful in here.
    circled: Vec<CellRef>,
    /// Whether the circles are being drawn at all.
    circle_invalid: bool,
    /// Circles actually PAINTED last frame. Real paint output, counted at the
    /// point of drawing, so a test asserting on it reads the screen rather
    /// than the model — and it counts the specific shape this feature adds
    /// rather than a total other effects also move.
    last_validation_circles: usize,

    /// The Goal Seek dialog, when it is open (issue #35).
    ///
    /// `None` is the common case and costs nothing: no dialog, no solver, and
    /// the paint path never looks at it.
    goal_seek: Option<GoalSeekState>,
    /// The Protect Sheet / Protect Workbook dialog (issue #42), when open.
    protect_dialog: Option<crate::protect_panel::ProtectDialog>,

    /// The command palette (issue #40).
    ///
    /// Closed is the overwhelmingly common case and costs nothing per frame.
    /// Note the name: `palette` alone means the COLOUR palette here (`theme`,
    /// issue #19), so this one is always spelled out.
    command_palette: crate::command::CommandPalette,

    /// Persisted preferences, written back whenever a toggle flips.
    prefs: Prefs,

    // ---- recent files / session restore (issue #45) ----
    //
    // Three small fields rather than a sub-struct: they are read from the
    // load path and the menu bar, and an extra level of indirection would buy
    // nothing.
    /// The file currently open, and therefore the workbook half of the
    /// `(workbook path, sheet name)` zoom key. `None` for a workbook that has
    /// never been saved, which keys against the empty path.
    source_path: Option<PathBuf>,
    /// The path a load in flight is for, promoted to `source_path` only when
    /// that load actually succeeds — a failed open must not rewrite history.
    pending_path: Option<PathBuf>,
    /// Show the start screen instead of the grid. True on a cold start with
    /// no file argument; any choice on that screen turns it off.
    show_start: bool,
    /// Whether the user has a workbook in front of them yet (issue #52).
    ///
    /// Set the moment anything produces one — a file load, "Blank workbook",
    /// a template, or adding a sheet — and never cleared except by returning
    /// to the start screen. It is what separates "nothing opened yet", which
    /// gets the "Open a CSV" placeholder, from "an empty workbook the user
    /// asked for", which must be a typeable grid. Deriving that from
    /// `row_count == 0` instead is exactly the conflation that made a new
    /// sheet swallow every keystroke.
    workbook_started: bool,

    /// The import wizard (issue #31), when a file did not parse cleanly and
    /// the user has not yet chosen settings for it.
    ///
    /// `None` is the overwhelmingly common case — a well-formed UTF-8 comma
    /// CSV never opens this — and costs one `Option` check per frame.
    import_wizard: Option<crate::import_wizard::ImportWizard>,
}

/// The Goal Seek dialog's state: three input fields, a result line, and the
/// rects of its buttons so the harness can click the REAL widgets.
#[derive(Default)]
pub struct GoalSeekState {
    /// "Set cell" — an A1 reference, as typed.
    pub set_cell: String,
    /// "To value" — a number, as typed.
    pub to_value: String,
    /// "By changing cell" — an A1 reference, as typed.
    pub by_changing: String,
    /// What to tell the user about the last run: the text, and whether it is
    /// a failure (rendered in the error colour).
    pub message: Option<(String, bool)>,
    /// Set once a run has actually COMMITTED an edit.
    ///
    /// This is what makes Cancel safe. Goal Seek's single undo entry is the
    /// only thing Cancel may rewind; without this flag, pressing Cancel on a
    /// dialog that never solved anything — or after a refusal, which commits
    /// nothing — would undo the user's previous, unrelated edit.
    pub applied: bool,
    /// Where the buttons were actually painted, so a test clicks the real
    /// widget rather than trusting a handler call. `None` until painted.
    pub solve_rect: Option<egui::Rect>,
    pub cancel_rect: Option<egui::Rect>,
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
            extra_selections: Vec::new(),
            scroll: ScrollState::default(),
            focus: Focus::Grid,
            editing: None,
            edit_buffer: String::new(),
            just_started_edit: false,
            formula_input: String::new(),
            formula_result: None,
            edit_pre_text: String::new(),
            edit_caret: 0,
            pending_caret: None,
            formula_bar_rows: prefs.formula_bar_rows,
            last_formula_bar_h: 0.0,
            show_formulas: std::collections::HashSet::new(),
            ref_outlines: Vec::new(),
            ref_drag: None,
            search_open: false,
            search_input: String::new(),
            search_results: ferrix_core::SearchResults::default(),
            search_index: 0,
            search_focus_pending: false,
            search_case_sensitive: false,
            search_whole_cell: false,
            chart: crate::chart_panel::ChartPanel::default(),
            search_filter_mode: false,
            replace_open: false,
            replace_input: String::new(),
            replace_focus_pending: false,
            search_regex: false,
            search_regex_error: None,
            replace_look_in: ferrix_core::LookIn::Values,
            replace_cancel: ferrix_core::CancelToken::new(),
            replace_progress: None,
            replace_cancel_after_applied: None,
            row_filter: None,
            sort_keys: Vec::new(),
            sort_order: None,
            subtotals: None,
            subtotal_spec: None,
            header_drag: None,
            sizing: ferrix_core::sizing::SheetSizing::new(),
            print_area: None,
            show_page_breaks: false,
            dragging_content: false,
            page_setup: ferrix_core::page::PageSetup::default(),
            break_drag: None,
            last_page_break_lines: 0,
            col_resize: None,
            header_menu: None,
            last_autofit_rows: 0,
            hidden_rows: None,
            hidden_cols: std::collections::BTreeSet::new(),
            sizing_dirty: false,
            clip_block: None,
            clip_html: None,
            paste_special: None,
            last_outline_buttons: 0,
            sizing_path: None,
            pivots_path: None,
            header_hitboxes: Vec::new(),
            row_header_hitboxes: Vec::new(),
            last_painted_rows: Vec::new(),
            last_frozen_rows: 0,
            last_grid_rect: None,
            fill_source: None,
            fill_target: None,
            move_origin: None,
            pending_block_move: None,
            tables: Vec::new(),
            table_mask: None,
            table_uniques: Vec::new(),
            table_report: ferrix_core::ValidationReport::default(),
            edits_path: None,
            fingerprint: None,
            cache_path: None,
            cache_headers: Vec::new(),
            compacting: false,
            compact_rx: None,
            compact_progress_rx: None,
            compact_progress: Progress::default(),
            compact_cancel: None,
            compact_started: None,
            autosave_last: None,
            autosave_revision: None,
            autosave_rx: None,
            recovery: None,
            recovery_resolved: false,
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
            comments_path: None,
            last_comment_markers: 0,
            last_border_segments: 0,
            last_rotated_texts: 0,
            last_wrapped_texts: 0,
            last_subtotal_rows: 0,
            last_subtotal_texts: 0,
            last_sparkline_shapes: 0,
            last_sparkline_blanks: 0,
            trace: None,
            last_trace_arrows: 0,
            last_trace_total: 0,
            comment_editing: None,
            comment_buffer: String::new(),
            comment_author_buffer: String::new(),
            comment_focus_pending: false,
            context_cell: None,
            context_pos: egui::Pos2::ZERO,
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
            panes: crate::grid::Panes::default(),
            // The first sheet is named before any file is opened, so its
            // remembered zoom is adopted here and re-adopted on every sheet
            // switch — a zoom is a property of the sheet, not of the session.
            // No file is open yet, so this keys against the empty path — the
            // same bucket a scratch workbook uses (issue #45).
            zoom: prefs.zoom_of(std::path::Path::new(""), "Sheet1"),
            name_box_edit: None,
            names_open: false,
            name_edit: None,
            name_edit_ident: String::new(),
            name_edit_target: String::new(),
            name_error: None,
            name_box_sheet_scope: false,
            cond: None,
            cond_warning: None,
            validation: None,
            autocomplete: crate::validation_panel::AutocompleteState::default(),
            autocomplete_on: true,
            dropdown_button: None,
            circled: Vec::new(),
            circle_invalid: false,
            last_validation_circles: 0,
            goal_seek: None,
            protect_dialog: None,
            // Recency is restored before the first frame, so the palette's
            // ranking is right the first time it is opened rather than after
            // the first command of the session.
            command_palette: {
                let mut p = crate::command::CommandPalette::default();
                p.set_recent_slugs(&prefs.recent_commands);
                p
            },
            prefs,
            source_path: None,
            pending_path: None,
            // A file on the command line goes straight to the grid; only a
            // bare launch has nothing to show yet.
            show_start: false,
            workbook_started: false,
            import_wizard: None,
        };
        if let Some(p) = initial {
            app.start_load(p);
        } else {
            app.show_start = true;
        }
        app
    }

    /// Open a file, showing the import wizard first when it will not parse
    /// cleanly (issue #31).
    ///
    /// Order matters and is the whole feature:
    ///
    /// 1. A REMEMBERED rule for this file name wins outright — that is what
    ///    "remember these settings" buys, and consulting detection first
    ///    would re-raise the wizard on a file the user already configured.
    /// 2. Otherwise detection runs over a BOUNDED PREFIX. A 10GB file reaches
    ///    this decision in the time it takes to read 128 KB.
    /// 3. A clean file loads immediately, exactly as before. Only a file that
    ///    would load as nonsense stops for the wizard.
    ///
    /// Detection is deliberately synchronous: it is a bounded read, so it
    /// cannot be the thing that blocks the UI, and making it async would let
    /// a frame paint an empty grid before the wizard appeared.
    fn start_load(&mut self, path: PathBuf) {
        if let Some(opts) = self.import_options_for(&path) {
            self.start_load_with(path, opts);
        }
    }

    /// Options to open `path` with, or `None` when the wizard took over.
    fn import_options_for(&mut self, path: &Path) -> Option<CsvOptions> {
        // Only delimited text has import settings. xlsx/parquet/arrow carry
        // their own schema, and sniffing them would produce a wizard offering
        // to change the delimiter of a binary file.
        if !is_delimited_path(path) {
            return Some(CsvOptions::default());
        }
        if let Some(rule) = self.prefs.import_rule_for(path) {
            return Some(rule.to_options());
        }
        match ferrix_io::sniff_path(path) {
            Ok(d) if d.clean => Some(d.to_options()),
            Ok(d) => {
                self.import_wizard =
                    Some(crate::import_wizard::ImportWizard::from_detection(path, &d));
                self.show_start = false;
                self.status = format!(
                    "{} needs import settings — {}",
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    d.reason.unwrap_or_default()
                );
                None
            }
            // Unreadable at detection time: let the real load report the
            // error properly rather than inventing a wizard for a file that
            // does not open.
            Err(_) => Some(CsvOptions::default()),
        }
    }

    /// Kick off a load on a worker thread so the UI never blocks — converting
    /// a 10GB file takes minutes and the window must stay responsive.
    fn start_load_with(&mut self, path: PathBuf, opts: CsvOptions) {
        let (tx, rx) = channel();
        let (ptx, prx) = channel::<Progress>();
        let cancel = ferrix_core::CancelToken::new();
        let mut should_cancel = cancel.checker();
        self.loading = true;
        self.progress = Progress::default();
        self.status = format!("Opening {}…", path.display());
        // Recorded, not adopted: `source_path` only moves when the load lands.
        self.pending_path = Some(path.clone());

        std::thread::spawn(move || {
            let result = load_any(
                &path,
                opts,
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
                self.comments_path = loaded.comments_path;
                let loaded_comments = loaded.comments;
                self.cache_path = loaded.cache_path;
                self.cache_headers = match &loaded.base {
                    BaseData::Mapped(m) => m.headers().to_vec(),
                    BaseData::Memory(_) => Vec::new(),
                };
                // Offer recovery before anything else touches the overlay.
                self.recovery = loaded.recovery;
                self.recovery_resolved = false;
                // A fresh dataset resets the autosave timer and its
                // change-tracking baseline.
                self.autosave_last = None;
                self.autosave_revision = None;
                let restored_count = loaded.restored.as_ref().map(|o| o.len());
                self.wb = build_workbook(
                    loaded.base,
                    loaded.sheet_name,
                    loaded.first_formulas,
                    loaded.restored,
                    loaded.extra_sheets,
                    loaded.names,
                );
                // Adopted AFTER build_workbook, which constructs a fresh
                // Workbook and would otherwise discard them.
                let restored_comments = loaded_comments.len();
                self.wb.comments = loaded_comments;
                // Protection read from the file, for the same reason (issue
                // #42). Only the ACTIVE sheet's is adopted, matching how
                // comments and merges are handled; the workbook-structure
                // flags are global and always apply. Honouring the flags we
                // find is the whole point of the issue — an imported
                // protected sheet must not silently become editable.
                self.wb.adopt_protection(
                    loaded
                        .protection
                        .iter()
                        .find(|(i, _)| *i == 0)
                        .map(|(_, p)| p.clone()),
                    loaded.wb_protection,
                );
                if let Some(w) = loaded.edit_warning {
                    self.status = format!("Saved edits not applied — {w}");
                } else if let Some(n) = restored_count {
                    self.status = format!("{} · restored {} saved edits", self.status, fmt_int(n));
                }
                if restored_comments > 0 {
                    self.status = format!(
                        "{} · {} comment{}",
                        self.status,
                        fmt_int(restored_comments),
                        if restored_comments == 1 { "" } else { "s" }
                    );
                }
                self.selection.move_to(CellRef::new(0, 0));
                self.scroll = ScrollState::default();
                self.panes = crate::grid::Panes::default();
                // The load succeeded, so this file is now the open one.
                self.source_path = self.pending_path.take();
                // Sizing lives beside the base file (issue #29). Loaded right
                // after the source path is known, so the first painted frame
                // already has the user's widths, hidden spans and outline —
                // rather than painting defaults and snapping a frame later.
                self.sizing_path = self.source_path.as_deref().map(ferrix_io::sizing_path_for);
                self.set_sizing(ferrix_core::sizing::SheetSizing::new());
                self.load_sizing_sidecar();
                // Pivot bindings (issue #33 Part B): same timing and rationale
                // as sizing — the path is known now, so a pivot sheet paints its
                // computed result on the first frame instead of blank cells.
                self.pivots_path = self.source_path.as_deref().map(ferrix_io::pivot_path_for);
                self.load_pivots_sidecar();
                // The workbook we just built has the FILE's sheet name, which
                // is only known now — so this is where the sheet's remembered
                // zoom is adopted. Doing it at construction would read the
                // placeholder "Sheet1" and silently lose the preference.
                // Keyed on the workbook too since #45, so two files that both
                // call their first sheet "Sheet1" no longer share one zoom.
                self.zoom = self.prefs.zoom_of(&self.book_key(), self.wb.active_name());
                // Recent list + session restore (issue #45). The entry is
                // touched first so a brand-new file has somewhere to restore
                // from, then the session it already had is applied.
                if let Some(p) = self.source_path.clone() {
                    crate::recent::touch(&mut self.prefs.recent, &p);
                    let session = crate::recent::session_of(&self.prefs.recent, &p);
                    self.apply_session(&session);
                    self.persist_prefs();
                }
                self.show_start = false;
                self.workbook_started = true;
                self.loading = false;
                self.load_rx = None;
                self.progress_rx = None;
                self.load_cancel = None;
                self.sync_formula_bar();
            }
            Ok(Err(e)) => {
                // The open failed, so nothing about the current file changes.
                self.pending_path = None;
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
            .add_filter(
                "Spreadsheets",
                &[
                    "csv", "tsv", "txt", "xlsx", "parquet", "pq", "arrow", "feather",
                ],
            )
            .add_filter("CSV", &["csv", "tsv", "txt"])
            .add_filter("Excel", &["xlsx"])
            // Same list `ferrix_io::format_for_path` routes on, so the dialog
            // cannot offer a file the open path then refuses.
            .add_filter("Parquet / Arrow", ferrix_io::ARROW_EXTENSIONS)
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
        // A pivot sheet's cells are computed from a spec, not typed (issue #33
        // Part B). Say so up front — like the spilled-cell hint below — rather
        // than letting the commit be refused later with only a generic locked
        // message. The commit path still refuses the write (guard_edit); this is
        // the explanation the acceptance criterion asks for.
        if self.wb.is_pivot_sheet(self.wb.active_sheet()) {
            self.status = format!(
                "{} is a pivot table cell — refresh or change the pivot instead of editing it",
                cell.to_a1()
            );
        }
        // A spilled cell is owned by its host formula and refuses direct edits
        // (#27 P2). Say so up front rather than letting the commit fail
        // silently-looking later — the value the user sees is a projection of
        // the host's array, not something they can type over.
        if self.wb.is_spilled_cell(cell) {
            self.status = format!(
                "{} is part of a spilled array — edit its source formula instead",
                cell.to_a1()
            );
        }
        // Captured BEFORE the seed is applied. When the edit began by typing
        // over the cell, the seed is the single character the user pressed and
        // the cell's real text exists nowhere else by the time Escape arrives
        // — this is the only copy (issue #38).
        self.edit_pre_text = self.wb.view().edit_text(cell);
        self.editing = Some(cell);
        self.edit_buffer = seed.unwrap_or_else(|| self.wb.view().edit_text(cell));
        // The formula bar mirrors the cell editor while an edit is live, so
        // the multi-line bar is usable for the edit and so the user can see
        // the whole of a long formula they are typing into a narrow column.
        self.formula_input.clone_from(&self.edit_buffer);
        self.recompute_formula();
        self.edit_caret = self.edit_buffer.len();
        self.focus = Focus::Cell;
        self.just_started_edit = true;
    }

    /// Text being edited right now, and whether it lives in the CELL editor.
    ///
    /// One accessor for both editors, so F4, the reference outlines and the
    /// outline drag all act on whichever field the user is actually in rather
    /// than each keeping its own idea of "the formula".
    fn live_edit_text(&self) -> Option<(bool, String)> {
        if self.editing.is_some() {
            return Some((true, self.edit_buffer.clone()));
        }
        if self.focus == Focus::FormulaBar {
            return Some((false, self.formula_input.clone()));
        }
        None
    }

    /// Write back through the same door [`Self::live_edit_text`] read from,
    /// keeping the mirrored copy and the live preview in step.
    fn set_live_edit_text(&mut self, is_cell: bool, text: String) {
        if is_cell {
            self.edit_buffer.clone_from(&text);
        }
        self.formula_input = text;
        self.recompute_formula();
    }

    fn commit_edit(&mut self) {
        let Some(cell) = self.editing.take() else {
            return;
        };
        let raw = std::mem::take(&mut self.edit_buffer);
        self.autocomplete.reset();

        // --- data validation (issue #41) ---
        //
        // Checked BEFORE the write, on the raw text, at the same single
        // chokepoint protection uses. A `Stop` rule REJECTS: nothing is
        // written, the cell keeps what it had, and the message says why. A
        // `Warning` rule lets the value through and still says so — the
        // difference the acceptance criterion names, decided by one predicate
        // (`ErrorStyle::rejects`) so the two paths cannot drift.
        let verdict = self.wb.check_typed(cell, &raw);
        if let Some((style, message)) = &verdict {
            if style.rejects() {
                self.status = format!("{} not changed — {message}", cell.to_a1());
                self.focus = Focus::Grid;
                self.sync_formula_bar();
                return;
            }
        }

        let report = self.wb.commit_edit(cell, &raw);
        self.focus = Focus::Grid;

        // A Warning rule allowed the entry; the user must still be told.
        if let Some((style, message)) = &verdict {
            if !style.rejects() {
                self.status = format!("{} updated — warning: {message}", cell.to_a1());
                self.sync_formula_bar();
                // Recompute the rings so the newly-invalid cell is marked.
                self.refresh_circles();
                return;
            }
        }
        self.status = if let Some(denied) = &report.denied {
            // Issue #42: a refused edit MUST say why. Doing nothing silently
            // is the failure mode the acceptance criterion names, and it is
            // indistinguishable from a broken keyboard.
            format!("{} not changed — {denied}", cell.to_a1())
        } else if let Some(err) = &report.parse_error {
            format!("{}: {err}", cell.to_a1())
        } else if let Some(blocker) = self.wb.spill_blocker_at(cell) {
            // #27 P2: a blocked spill must not be a dead-end #SPILL!. Name the
            // cell that is in the way so the user knows what to clear.
            format!("{}: #SPILL! — blocked by {}", cell.to_a1(), blocker.to_a1())
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

    /// Escape. Puts back EXACTLY what was there before the edit started.
    ///
    /// The cell's stored value was never touched — nothing is committed until
    /// Enter — but the formula bar mirrors the edit buffer while an edit is
    /// live, so abandoning the edit has to restore the bar too. Restored from
    /// the snapshot rather than re-read from the cell, because the two are the
    /// same thing only when nothing else moved in between, and "restores what
    /// you had" should not depend on that.
    fn cancel_edit(&mut self) {
        self.editing = None;
        self.edit_buffer.clear();
        self.autocomplete.reset();
        self.formula_input = std::mem::take(&mut self.edit_pre_text);
        self.recompute_formula();
        self.ref_drag = None;
        self.focus = Focus::Grid;
    }

    /// Escape with no cell edit open, i.e. the user was typing in the formula
    /// bar itself. Same contract: the bar goes back to what it showed when it
    /// gained focus.
    fn cancel_formula_bar(&mut self) {
        self.formula_input = std::mem::take(&mut self.edit_pre_text);
        self.recompute_formula();
        self.ref_drag = None;
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

    // ---- freeze panes, split view, zoom (roadmap #6) ----

    /// The frozen / split band the grid should render.
    pub fn panes(&self) -> crate::grid::Panes {
        self.panes
    }

    /// Current zoom factor for the active sheet.
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Freeze rows above and/or columns left of the CURSOR.
    ///
    /// The cursor's own row is the first SCROLLING row, matching every
    /// spreadsheet: "freeze at row 5" means rows 1..4 stay put. The cursor's
    /// row is taken in SCREEN space, through the same resolver the grid paints
    /// with, so freezing under a sort or filter freezes the rows the user can
    /// actually see rather than an unrelated slice of the underlying sheet.
    pub fn freeze_at_cursor(&mut self, rows: bool, cols: bool) {
        let screen_row = self
            .row_resolver(self.pad_space())
            .visible_of(self.selection.cursor.row)
            .unwrap_or(0);
        self.panes = crate::grid::Panes::freeze(
            if rows { screen_row } else { self.panes.rows },
            if cols {
                self.selection.cursor.col as usize
            } else {
                self.panes.cols
            },
        );
        // The body cannot show rows the band already owns, so park it at the
        // first scrolling row rather than leaving it above the seam.
        self.scroll.row_offset = self.scroll.row_offset.max(self.panes.body_min_row());
        self.status = match (self.panes.rows, self.panes.cols) {
            (0, 0) => "Nothing to freeze — the cursor is at A1".into(),
            (r, 0) => format!("Froze {r} row{}", if r == 1 { "" } else { "s" }),
            (0, c) => format!("Froze {c} column{}", if c == 1 { "" } else { "s" }),
            (r, c) => format!("Froze {r} rows and {c} columns"),
        };
    }

    /// Drop the frozen band / split entirely.
    pub fn unfreeze(&mut self) {
        let had = self.panes.is_active();
        self.panes = crate::grid::Panes::default();
        self.status = if had {
            "Unfroze panes".into()
        } else {
            "No frozen panes".into()
        };
    }

    /// Split at the cursor: same band, but its offset is the user's to move.
    ///
    /// Two independent scroll offsets over ONE column layout — the split pane
    /// and the body index the same widths and the same rows, so they can never
    /// disagree about what a column is.
    pub fn split_at_cursor(&mut self) {
        let screen_row = self
            .row_resolver(self.pad_space())
            .visible_of(self.selection.cursor.row)
            .unwrap_or(0);
        self.panes = crate::grid::Panes {
            rows: screen_row,
            cols: self.selection.cursor.col as usize,
            frozen: false,
            lead_row: 0.0,
            lead_col: 0,
        };
        self.status = "Split view — the top pane scrolls on its own".into();
    }

    /// Set the zoom for the active sheet and remember it.
    pub fn set_zoom(&mut self, z: f32) {
        self.zoom = crate::grid::clamp_zoom(z);
        let name = self.wb.active_name().to_string();
        // Keyed on (workbook, sheet) since #45: without the workbook half,
        // every file whose first sheet is called "Sheet1" shared one zoom.
        let book = self.book_key();
        self.prefs.set_zoom(&book, &name, self.zoom);
        self.remember_session();
        self.persist_prefs();
        self.status = format!("Zoom {}%", (self.zoom * 100.0).round() as i32);
    }

    // ---- recent files, templates, session restore (issue #45) ----

    /// The workbook half of the zoom key, and the recent-list identity.
    ///
    /// An unsaved in-memory workbook has no file, and keys against the empty
    /// path — one shared bucket for scratch workbooks, which is right: they
    /// are not distinguishable from each other across a restart either.
    fn book_key(&self) -> PathBuf {
        self.source_path.clone().unwrap_or_default()
    }

    /// The file currently open, if any.
    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    /// Whether the start screen is showing rather than the grid.
    pub fn showing_start_screen(&self) -> bool {
        self.show_start
    }

    /// The recent-files list as persisted.
    pub fn recent(&self) -> &[crate::recent::RecentEntry] {
        &self.prefs.recent
    }

    /// The body pane's scroll offset, in fractional rows.
    pub fn scroll_row_offset(&self) -> f64 {
        self.scroll.row_offset
    }

    /// The active sheet's name — the sheet half of the zoom key.
    pub fn active_sheet_name(&self) -> &str {
        self.wb.active_name()
    }

    /// Where the user is right now, as a restorable session.
    pub fn current_session(&self) -> crate::recent::Session {
        crate::recent::Session {
            anchor: (self.selection.anchor.row, self.selection.anchor.col),
            cursor: (self.selection.cursor.row, self.selection.cursor.col),
            scroll_row: self.scroll.row_offset,
            scroll_col_px: self.scroll.col_px,
            frozen_rows: self.panes.rows,
            frozen_cols: self.panes.cols,
            frozen: self.panes.frozen,
        }
    }

    /// Put the user back where a previous visit to this file left them.
    ///
    /// Bounds are NOT clamped here: the row count may still be arriving on the
    /// load thread, and the grid clamps the scroll offset against the real
    /// extents every frame anyway. Clamping against a partial row count would
    /// pin a restored deep scroll to the top.
    fn apply_session(&mut self, s: &crate::recent::Session) {
        self.selection = ferrix_core::Selection::new(
            CellRef::new(s.anchor.0, s.anchor.1),
            CellRef::new(s.cursor.0, s.cursor.1),
        );
        self.scroll.row_offset = s.scroll_row;
        self.scroll.col_px = s.scroll_col_px;
        self.panes = crate::grid::Panes {
            rows: s.frozen_rows,
            cols: s.frozen_cols,
            frozen: s.frozen,
            lead_row: 0.0,
            lead_col: 0,
        };
    }

    /// Record the current session against the open file, in memory.
    ///
    /// Cheap and callable often: it writes one fixed-size record into a list
    /// capped at `recent::MAX_RECENT`. Persisting it to disk is the caller's
    /// separate `persist_prefs`, so a per-frame caller cannot cause a per-frame
    /// file write.
    pub fn remember_session(&mut self) {
        let Some(p) = self.source_path.clone() else {
            return;
        };
        let s = self.current_session();
        crate::recent::set_session(&mut self.prefs.recent, &p, s);
    }

    /// Record the session AND write it out. Called when the app is closing and
    /// on the interactions worth surviving a crash.
    pub fn persist_session(&mut self) {
        self.remember_session();
        self.persist_prefs();
    }

    /// Start an empty workbook from the start screen.
    pub fn new_blank_workbook(&mut self) {
        self.wb = Workbook::new(BaseData::Memory(Sheet::new("Sheet1")));
        self.reset_for_new_workbook();
        self.status = "Blank workbook".into();
    }

    /// Start a workbook seeded from `templates()[i]`.
    ///
    /// Literal cells are written into the base sheet, which is what gives the
    /// sheet its extent — an overlay entry outside the base's row count reads
    /// as empty, so committing everything through the overlay would produce a
    /// workbook that looks blank.
    ///
    /// Formulas then go through the SAME `commit_edit` path a user typing them
    /// would take, so they are parsed, graphed and evaluated by the real
    /// engine rather than by a second, drifting one.
    pub fn new_from_template(&mut self, index: usize) {
        let Some(t) = crate::recent::templates().get(index) else {
            return;
        };
        let mut sheet = Sheet::new(t.name);
        sheet.set_headers(t.headers.iter().map(|h| h.to_string()).collect());
        // Pass one: literals into the base, establishing the extent. A cell
        // that parses as a number is stored as one so the template's sums have
        // numbers to add rather than text.
        for (r, row) in t.rows.iter().enumerate() {
            for (c, text) in row.iter().enumerate() {
                let cell = CellRef::new(r as u32, c as u32);
                if text.starts_with('=') {
                    // Reserved by an empty value so the column exists and the
                    // row count covers it; the formula lands in pass two.
                    sheet.set(cell, ferrix_core::Value::Empty);
                } else if let Ok(n) = text.parse::<f64>() {
                    sheet.set(cell, ferrix_core::Value::Number(n));
                } else {
                    sheet.set_text(cell, text);
                }
            }
        }
        self.wb = Workbook::new(BaseData::Memory(sheet));
        self.reset_for_new_workbook();
        // Pass two: the formulas, through the real commit path.
        for (r, row) in t.rows.iter().enumerate() {
            for (c, text) in row.iter().enumerate() {
                if text.starts_with('=') {
                    self.wb.commit_edit(CellRef::new(r as u32, c as u32), text);
                }
            }
        }
        self.stats_rows = t.rows.len();
        self.stats_cols = t.headers.len();
        self.status = format!("New workbook from the {} template", t.name);
    }

    /// Shared reset for the two "start something new" paths.
    ///
    /// A new workbook has no file behind it, so it must NOT inherit the
    /// previous file's sidecar paths — writing this workbook's edits into the
    /// last file's sidecar would corrupt a file the user is not even looking
    /// at.
    fn reset_for_new_workbook(&mut self) {
        self.source_path = None;
        self.pending_path = None;
        self.show_start = false;
        // The user asked for a workbook, so this is no longer a cold start:
        // the grid must be typeable even when it holds nothing (issue #52).
        self.workbook_started = true;
        self.edits_path = None;
        self.fingerprint = None;
        self.comments_path = None;
        // Sizing belongs to the file that is closing. Leaving it would apply
        // the last file's widths and hidden rows to the next one — the same
        // class of bug as writing this workbook's edits into the previous
        // file's sidecar.
        self.sizing_path = None;
        self.set_sizing(ferrix_core::sizing::SheetSizing::new());
        self.sizing_dirty = false;
        // Pivot bindings belong to the file that is closing, same as sizing:
        // carrying them into the next workbook would apply one file's pivots to
        // another's data. The workbook's own pivot map is discarded with the
        // workbook itself when the fresh one below replaces it.
        self.pivots_path = None;
        // The print area is per sheet; a new file starts with none.
        self.print_area = None;
        self.show_page_breaks = false;
        // Page setup (including any manual page breaks) is per sheet.
        self.page_setup = ferrix_core::page::PageSetup::default();
        self.break_drag = None;
        self.cache_path = None;
        self.cache_headers = Vec::new();
        self.col_widths = Vec::new();
        self.stats_rows = 0;
        self.stats_cols = 0;
        self.selection = ferrix_core::Selection::default();
        self.scroll = ScrollState::default();
        self.panes = crate::grid::Panes::default();
        self.zoom = self.prefs.zoom_of(&PathBuf::new(), self.wb.active_name());
        self.autosave_last = None;
        self.autosave_revision = None;
        self.sync_formula_bar();
    }

    /// Act on a start-screen choice.
    pub fn take_start_choice(&mut self, choice: crate::recent::StartChoice) {
        match choice {
            crate::recent::StartChoice::Open(p) => self.start_load(p),
            crate::recent::StartChoice::Blank => self.new_blank_workbook(),
            crate::recent::StartChoice::Template(i) => self.new_from_template(i),
            crate::recent::StartChoice::Browse => {
                self.open_dialog();
                // The picker is modal and may have been cancelled; only an
                // actual load leaves the start screen.
                if self.loading {
                    self.show_start = false;
                }
            }
        }
    }

    pub fn zoom_in(&mut self) {
        self.set_zoom(crate::grid::zoom_in(self.zoom));
    }

    pub fn zoom_out(&mut self) {
        self.set_zoom(crate::grid::zoom_out(self.zoom));
    }

    pub fn zoom_reset(&mut self) {
        self.set_zoom(1.0);
    }

    // ---- trace precedents / dependents (roadmap #39) ----

    /// The active trace session, for the caller to paint arrows for.
    pub fn trace(&self) -> Option<crate::trace::TraceState> {
        self.trace
    }

    /// The workbook-wide dependency graph, for callers that want to ask
    /// questions about it directly (tests, and future trace UI).
    pub fn graph_snapshot(&self) -> &ferrix_formula::depgraph::DepGraph {
        &self.wb.graph
    }

    /// The active sheet's id, for tests that need to build a `SheetCell`.
    pub fn active_sheet_id(&self) -> ferrix_core::SheetId {
        self.wb.active_sheet()
    }

    /// Arrows painted by the last frame, and how many the current trace
    /// level covers before the cap — the "showing N of M" note.
    pub fn trace_counts(&self) -> (usize, usize) {
        (self.last_trace_arrows, self.last_trace_total)
    }

    fn origin_cell(&self) -> ferrix_core::SheetCell {
        ferrix_core::SheetCell::new(self.wb.active_sheet(), self.selection.cursor)
    }

    /// Trace Precedents on the cursor cell. A second press on the SAME
    /// origin+direction walks one level further out, matching Excel; a press
    /// on a different cell (or the other direction) starts a fresh trace.
    pub fn trace_precedents(&mut self) {
        self.trace_step(crate::trace::TraceKind::Precedents);
    }

    /// Trace Dependents on the cursor cell. Same one-level-further-out
    /// behaviour as `trace_precedents`.
    pub fn trace_dependents(&mut self) {
        self.trace_step(crate::trace::TraceKind::Dependents);
    }

    fn trace_step(&mut self, kind: crate::trace::TraceKind) {
        let origin = self.origin_cell();
        match &mut self.trace {
            Some(t) if t.origin == origin && t.kind == kind => {
                t.depth += 1;
            }
            _ => {
                self.trace = Some(crate::trace::TraceState::new(origin, kind));
            }
        }
        let label = match kind {
            crate::trace::TraceKind::Precedents => "Precedents",
            crate::trace::TraceKind::Dependents => "Dependents",
        };
        let depth = self.trace.map(|t| t.depth).unwrap_or(1);
        self.status = format!(
            "Tracing {label} of {} ({depth} level{})",
            cell_label(origin.cell),
            if depth == 1 { "" } else { "s" }
        );
    }

    /// Remove Arrows: clears the active trace. Changing the selection does
    /// NOT do this implicitly — arrows must not strand themselves silently,
    /// but they also must not vanish just because the user glanced at
    /// another cell, so an explicit clear is the only way out.
    pub fn clear_trace(&mut self) {
        if self.trace.take().is_some() {
            self.status = "Removed trace arrows".into();
        }
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

    /// Padding rows the viewport offers past the last data row.
    ///
    /// Two sources, one number, so the pad the grid PAINTS and the pad
    /// `pad_space` HIT-TESTS can never disagree:
    ///
    /// - the "show empty rows" preference (issue #20), and
    /// - a sheet with NO BASE DATA, which gets the padding unconditionally
    ///   (issue #52).
    ///
    /// The second condition is on the BASE, not on the view. A sheet the user
    /// created holds no file data ever — its whole extent comes from the
    /// overlay — so "past the end of the sheet" is not a place the toggle
    /// should be able to take away: with zero rows there is nothing to click,
    /// nothing to put a cursor in, and a typed value has nowhere to go, which
    /// is exactly how a new sheet came to swallow edits in silence. Keying it
    /// on `view.row_count()` instead would hand the user one row of padding,
    /// let them type in it, and then take the padding away again — a sheet
    /// you can type into exactly once.
    ///
    /// A LOADED sheet keeps the old behaviour precisely, because its base has
    /// rows: the toggle alone decides.
    ///
    /// Viewport only, both ways: `view.row_count()` is untouched, so export,
    /// SUM and the status bar still see the real sheet.
    fn pad_rows(&self) -> usize {
        if self.wb.base.row_count() == 0 {
            return crate::grid::EMPTY_ROW_PADDING;
        }
        if self.show_empty_rows {
            crate::grid::EMPTY_ROW_PADDING
        } else {
            0
        }
    }

    /// Columns the cursor may reach. A sheet with no base columns offers a
    /// blank page's worth (issue #52) — the column mirror of
    /// [`Self::pad_rows`], and on the base for the same reason.
    fn navigable_cols(&self) -> usize {
        let view = self.wb.view().col_count();
        if self.wb.base.col_count() == 0 {
            return view.max(crate::grid::BLANK_SHEET_COLS);
        }
        view
    }

    /// True only for the launch state that has no workbook to show: nothing
    /// opened, nothing created, nothing typed.
    ///
    /// The distinction issue #52 turns on. "Zero rows" alone is NOT this
    /// state — a sheet the user just added, and a workbook they explicitly
    /// asked to start blank, also have zero rows, and both must be usable
    /// grids rather than a placeholder telling them to open a file.
    fn is_cold_start(&self) -> bool {
        !self.workbook_started && self.wb.view().row_count() == 0
    }

    /// The padding rows currently on offer, or `None` when there are none.
    ///
    /// `first_pad_screen_row` is the count of rows the FILTERS resolve, so
    /// padding always begins after the last row either filter kept, and
    /// `first_pad_data_row` is one past the end of the whole sheet — never
    /// past the filtered subset, which would alias onto hidden records.
    fn pad_space(&self) -> Option<crate::grid::PadSpace> {
        if self.pad_rows() == 0 {
            return None;
        }
        let view = self.wb.view();
        let data_rows = view.row_count().max(1);
        let filtered = match (&self.sort_order, &self.row_filter) {
            // A sort is built over the filtered rows, so its length already
            // accounts for both — padding still begins after the last row the
            // view resolves.
            (Some(s), _) => s.len(),
            (None, Some(f)) => f.len(),
            (None, None) => match (self.tables.first(), &self.table_mask) {
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
        (rows + self.pad_rows()).saturating_sub(1) as i64
    }

    fn move_selection_ext(&mut self, drow: i64, dcol: i64, extend: bool) {
        if self.editing.is_some() {
            self.commit_edit();
        }
        // Navigation may reach into the empty padding when the toggle is on,
        // and ALWAYS on an empty sheet (issue #52); the sheet's own extent is
        // unchanged either way.
        let max_row = self.max_navigable_row();
        let max_col = self.navigable_cols().saturating_sub(1) as i64;
        // Vertical movement is in VISIBLE rows under a filter: pressing Down
        // must land on the next row the user can actually see, not on a hidden
        // neighbour. The result is converted straight back to an underlying
        // row, so every downstream consumer keeps working in real addresses.
        // A SORT reorders the same rows, so "down" is equally not "+1" under
        // one: pressing Down must land on the row drawn beneath, which under a
        // sort is an arbitrary underlying row. Both transforms are handled by
        // the same visible-space arithmetic.
        let r = match (&self.sort_order, &self.row_filter, drow) {
            (Some(s), _, d) if d != 0 && !s.is_empty() => {
                let here = s
                    .visible_of(self.selection.cursor.row)
                    .unwrap_or_else(|| s.visible_at_or_after(self.selection.cursor.row))
                    as i64;
                let target = (here + d).clamp(0, s.len() as i64 - 1);
                s.underlying(target as usize).unwrap_or(0) as i64
            }
            (None, Some(f), d) if d != 0 && !f.is_empty() => {
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

    /// Copy the selection to the system clipboard.
    ///
    /// # Clipboard flavours, and the platform limit (issue #30)
    ///
    /// Excel puts SEVERAL flavours on the clipboard at once — plain TSV and an
    /// HTML `<table>` — and the receiver takes the richest one it understands.
    /// Ferrix cannot: **eframe's clipboard API is plain text only**
    /// (`Context::copy_text` takes a `String`), with no way to register
    /// `CF_HTML` beside `CF_UNICODETEXT`.
    ///
    /// So what actually goes on the system clipboard is TSV, exactly as
    /// before, which is what keeps Excel and every other consumer working. The
    /// rich HTML rendering is built and kept in [`Self::clip_html`] for the
    /// in-process round trip, and the paste path reads HTML whenever the text
    /// arriving looks like a table — so pasting rich content FROM Excel works
    /// while copying rich content TO Excel does not.
    ///
    /// Everything needed for the other half already exists and is unit tested
    /// in `ferrix_core::clipboard`; only this one call has to change the day
    /// eframe grows a flavoured clipboard or a native clipboard crate is added
    /// deliberately.
    fn copy_selection(&mut self, ctx: &egui::Context, cut: bool) {
        let sel = self.selection;
        let limit = self.max_block_cells();
        let widths = self.sizing.cols.clone();
        let Some(clip) = self.wb.copy_clip_block(sel, limit, |c| widths.width_of(c)) else {
            self.status = format!(
                "{} cells is too many to copy — {} fit in the memory available now",
                fmt_int(sel.cell_count() as usize),
                fmt_int(limit as usize)
            );
            return;
        };
        let tsv = ferrix_core::tsv::to_tsv(&clip.to_text_grid());
        // Held so a paste in this same window can read the rich flavour the
        // text clipboard cannot carry. Replaced wholesale on every copy, so it
        // can never be stale relative to what the user last copied.
        self.clip_html = Some(ferrix_core::clipboard::to_html(&clip));
        self.clip_block = Some(clip);
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

    /// Move the current selection's block by (d_row, d_col) as one undo step,
    /// and carry the selection to the block's new home (#82). The drag gesture
    /// and any menu/keyboard mover both call this; it owns the meaning.
    ///
    /// A MOVE (not a copy). If the destination would overwrite non-empty cells
    /// the block does not itself vacate, this parks the gesture and raises the
    /// confirmation modal instead of clobbering silently — the user answers via
    /// [`Self::confirm_block_move`] / [`Self::cancel_block_move`].
    pub fn move_selection_block(&mut self, d_row: i64, d_col: i64) {
        self.request_block_move(d_row, d_col, false);
    }

    /// Ctrl-drag: COPY the current selection's block to (d_row, d_col) rather
    /// than moving it (#82). Same overwrite-prompt discipline as a move.
    pub fn copy_selection_block(&mut self, d_row: i64, d_col: i64) {
        self.request_block_move(d_row, d_col, true);
    }

    /// Shared entry for both gestures: prompt first if the drop would overwrite
    /// data, otherwise carry it out at once.
    fn request_block_move(&mut self, d_row: i64, d_col: i64, copy: bool) {
        if d_row == 0 && d_col == 0 {
            return;
        }
        let sel = self.selection;
        if self.wb.block_move_would_overwrite(sel, d_row, d_col, copy) {
            self.pending_block_move = Some(PendingBlockMove {
                sel,
                d_row,
                d_col,
                copy,
            });
            return;
        }
        self.apply_block_move(sel, d_row, d_col, copy);
    }

    /// Carry out a (possibly already-confirmed) block move/copy, and move the
    /// selection to the block's new home so the user can chain another gesture
    /// or undo in one step.
    fn apply_block_move(&mut self, sel: Selection, d_row: i64, d_col: i64, copy: bool) {
        let limit = self.max_overlay_cells();
        match self.wb.move_block(sel, d_row, d_col, copy, limit) {
            Ok(0) => {}
            Ok(n) => {
                let (tl, br) = sel.bounds();
                let moved = Selection::new(
                    CellRef::new(
                        (tl.row as i64 + d_row) as u32,
                        (tl.col as i64 + d_col) as u32,
                    ),
                    CellRef::new(
                        (br.row as i64 + d_row) as u32,
                        (br.col as i64 + d_col) as u32,
                    ),
                );
                self.selection = moved;
                let verb = if copy { "Copied" } else { "Moved" };
                self.status = format!("{verb} {} cells", fmt_int(n));
                self.sync_formula_bar();
            }
            Err(e) => self.status = e,
        }
    }

    /// True while the block-move overwrite confirmation modal is up (#82).
    pub fn block_move_prompt_open(&self) -> bool {
        self.pending_block_move.is_some()
    }

    /// Answer "Replace" to the overwrite prompt: carry out the parked move/copy.
    pub fn confirm_block_move(&mut self) {
        if let Some(p) = self.pending_block_move.take() {
            self.apply_block_move(p.sel, p.d_row, p.d_col, p.copy);
        }
    }

    /// Answer "Cancel" to the overwrite prompt: drop the parked gesture and
    /// leave every cell exactly as it was.
    pub fn cancel_block_move(&mut self) {
        if self.pending_block_move.is_some() {
            self.pending_block_move = None;
            self.status = "Move cancelled".into();
        }
    }

    /// Paste whatever arrived from the clipboard at the selection's top-left.
    ///
    /// Prefers the HTML flavour: if the incoming text is an HTML table (what
    /// Excel and browsers put on the clipboard) it is parsed as one, keeping
    /// number formats and styling; otherwise it falls back to TSV, so the
    /// plain-text path that always worked still works.
    fn paste_clipboard(&mut self, text: &str) {
        self.paste_clipboard_with(text, ferrix_core::clipboard::PasteOptions::plain());
    }

    /// The Paste Special path, and what plain Ctrl+V delegates to.
    fn paste_clipboard_with(&mut self, text: &str, opts: ferrix_core::clipboard::PasteOptions) {
        // The richest payload available, in preference order: the block this
        // window last copied (which carries formats the text clipboard cannot
        // hold), then an HTML table if the incoming text is one, then TSV.
        //
        // The in-process block is used only when the text clipboard still
        // holds what that copy put there — otherwise the user copied something
        // in ANOTHER application since, and pasting our stale block would
        // silently ignore what they actually copied.
        let ours = self
            .clip_block
            .as_ref()
            .filter(|b| ferrix_core::tsv::to_tsv(&b.to_text_grid()) == text)
            .cloned();
        let block = match ours {
            Some(b) => b,
            None => ferrix_core::clipboard::parse_clipboard(text),
        };
        if block.is_empty() {
            self.status = "Clipboard is empty".into();
            return;
        }
        let origin = self.selection.bounds().0;
        let limit = self.max_overlay_cells();
        match self.wb.paste_special(origin, &block, opts, limit) {
            Ok(report) => {
                // Column widths live in the sizing model, so the workbook hands
                // them back rather than applying them itself.
                for (col, w) in &report.col_widths {
                    self.set_col_width(*col as usize, *w);
                }
                let (rows, cols) = if opts.transpose {
                    (block.cols() as u32, block.rows() as u32)
                } else {
                    (block.rows() as u32, block.cols() as u32)
                };
                // Select what was pasted, so the user sees the affected region
                // and can undo or overwrite it in one gesture.
                self.selection = Selection::new(
                    origin,
                    CellRef::new(
                        origin.row + rows.saturating_sub(1),
                        origin.col + cols.saturating_sub(1),
                    ),
                );
                self.status = paste_status(&report, opts);
                self.sync_formula_bar();
            }
            Err(e) => self.status = e,
        }
    }

    // ==================================== Paste Special (issue #30) ========
    //
    // These are the entry points the command palette and the headless harness
    // both drive, so a test exercises the same code the menu item does rather
    // than a parallel implementation of it.

    /// The rich HTML flavour of the last copy, if there was one.
    ///
    /// Exposed so a test can assert that the copy path really rendered a
    /// `<table>` — see the platform note on [`Self::copy_selection`] for why
    /// this cannot reach the system clipboard.
    pub fn clipboard_html(&self) -> Option<&str> {
        self.clip_html.as_deref()
    }

    /// The rich payload of the last copy.
    pub fn clipboard_block(&self) -> Option<&ferrix_core::clipboard::ClipBlock> {
        self.clip_block.as_ref()
    }

    /// Paste the clipboard with an explicit Paste Special request.
    ///
    /// `text` is what the system clipboard holds; the app prefers its own
    /// richer copy when that text still matches, and otherwise reads the HTML
    /// or TSV flavour out of it.
    pub fn paste_special(&mut self, text: &str, opts: ferrix_core::clipboard::PasteOptions) {
        self.paste_clipboard_with(text, opts);
    }

    /// Is the Paste Special dialog open?
    pub fn paste_special_is_open(&self) -> bool {
        self.paste_special.is_some()
    }

    /// Open the Paste Special dialog with a default request.
    pub fn paste_special_open(&mut self) {
        self.paste_special = Some(ferrix_core::clipboard::PasteOptions::plain());
    }

    pub fn paste_special_close(&mut self) {
        self.paste_special = None;
    }

    /// The number format a cell resolves to, as the painter sees it.
    ///
    /// Goes through `SheetFormat::number_format`, so it answers with the real
    /// precedence — cell override over range over column — rather than
    /// re-deriving it and possibly disagreeing with what is drawn.
    pub fn number_format_at(&self, cell: CellRef) -> Option<ferrix_core::NumberFormat> {
        self.wb.format.number_format(cell).cloned()
    }

    /// The style a cell resolves to, as the painter sees it.
    pub fn style_at(&self, cell: CellRef) -> ferrix_core::CellStyle {
        let mut plan = Vec::new();
        self.wb.format.plan(cell.col, &mut plan);
        let view = self.wb.view();
        let value = view.get(cell);
        let text = view.display(cell);
        self.wb.format.resolve(cell, &value, &text, &plan, &[])
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
        // Zoomed row height: at 200% half as many rows fit, and PageDown /
        // scroll-to-selection must move by what is actually on screen.
        ((self.last_viewport_h.max(200.0)) / crate::grid::Metrics::new(self.zoom).row_h) as usize
    }

    /// Persist edits to the sidecar beside the base file.
    ///
    /// Cheap by construction: only the overlay is written, so saving a handful
    /// of edits over a 200M-row dataset writes a handful of kilobytes and
    /// never touches the base.
    /// Returns true if edits were actually written to disk.
    fn save_edits(&mut self) -> bool {
        // Written first and unconditionally: the edits path below returns
        // early when there is nothing in the overlay, and a session that only
        // added a comment must not lose it to that early return.
        let comments_written = self.save_comments();
        // Sizing, for exactly the same reason: resizing a column leaves the
        // overlay empty, so it must be written before any early return.
        let sizing_written = self.save_sizing();
        // Pivot bindings, same reason again (issue #33 Part B): defining a pivot
        // leaves the overlay empty, so it must be persisted before the early
        // return below or a pivot-only session would lose its binding.
        self.save_pivots();
        let comment_count = self.wb.comments.len();
        let (Some(path), Some(fp)) = (self.edits_path.clone(), self.fingerprint) else {
            self.status = "Nothing to save — no file is open".into();
            return false;
        };
        if self.wb.overlay.is_empty() && !self.wb.is_dirty() {
            self.status = if comments_written && comment_count > 0 {
                format!(
                    "Saved {} comment{}",
                    fmt_int(comment_count),
                    if comment_count == 1 { "" } else { "s" }
                )
            } else if sizing_written && !self.sizing.is_empty() {
                "Saved row and column sizing".into()
            } else {
                "No edits to save".into()
            };
            return (comments_written && comment_count > 0)
                || (sizing_written && !self.sizing.is_empty());
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
                // The sidecar now holds everything the autosave did, so the
                // autosave is obsolete. Leaving it would make the next launch
                // offer to "recover" edits that are already saved.
                self.clear_autosave();
                true
            }
            Err(e) => {
                self.status = format!("Save failed: {e}");
                false
            }
        }
    }

    // --- autosave and crash recovery (roadmap #8) ---

    /// Drive the autosave timer. Called once per frame.
    ///
    /// Writes nothing unless all of: autosave is enabled, a file is open, the
    /// interval has elapsed, no write is already in flight, and the overlay
    /// has actually changed since the last successful autosave. That last
    /// condition is the "skip the write entirely when nothing changed" rule,
    /// and it is an integer comparison rather than an overlay diff.
    fn tick_autosave(&mut self) {
        self.poll_autosave();

        if !self.prefs.autosave_enabled() {
            return;
        }
        let (Some(path), Some(fp)) = (self.edits_path.clone(), self.fingerprint) else {
            return;
        };
        // Never autosave over an unanswered recovery prompt: the overlay on
        // screen is the pre-recovery state, and writing it would destroy the
        // very edits being offered.
        if self.recovery.is_some() {
            return;
        }
        // One write at a time. Two concurrent writers would race onto the
        // same path, and the loser's rename could resurrect older edits.
        if self.autosave_rx.is_some() {
            return;
        }

        let now = std::time::Instant::now();
        let due = match self.autosave_last {
            // First tick after a load starts the clock rather than firing;
            // opening a file should not immediately write one.
            None => {
                self.autosave_last = Some(now);
                false
            }
            Some(last) => now.duration_since(last) >= self.prefs.autosave_interval(),
        };
        if !due {
            return;
        }
        self.autosave_last = Some(now);

        let revision = self.wb.overlay.revision();
        // Nothing typed since the last autosave: no write at all. Not a
        // rewrite of identical bytes — no file touched, no mtime moved.
        if self.autosave_revision == Some(revision) {
            return;
        }
        // An empty overlay with nothing ever autosaved has nothing to protect.
        if self.wb.overlay.is_empty() && self.autosave_revision.is_none() {
            return;
        }

        self.spawn_autosave(path, fp, revision);
    }

    /// Clone the overlay and write it on a worker thread.
    ///
    /// The clone is what keeps the UI thread free: serializing a large overlay
    /// inline would stall the frame, and the overlay is `Clone` precisely so
    /// long-running work can take a snapshot and let editing continue.
    fn spawn_autosave(
        &mut self,
        path: PathBuf,
        fp: ferrix_io::edits::BaseFingerprint,
        revision: u64,
    ) {
        let snapshot = self.wb.overlay.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = ferrix_io::edits::write_autosave(&path, &snapshot, fp)
                .map(|bytes| (bytes, revision))
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.autosave_rx = Some(rx);
    }

    /// Collect a finished background autosave.
    ///
    /// The revision is recorded only on success, so a failed write is retried
    /// at the next tick rather than being mistaken for "already saved".
    fn poll_autosave(&mut self) {
        let Some(rx) = &self.autosave_rx else { return };
        match rx.try_recv() {
            Ok(Ok((_bytes, revision))) => {
                self.autosave_revision = Some(revision);
                self.autosave_rx = None;
            }
            Ok(Err(e)) => {
                // Surfaced, not swallowed: a user who believes autosave is
                // protecting them when it is not is worse off than one who
                // knows it is failing.
                self.status = format!("Autosave failed: {e}");
                self.autosave_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.autosave_rx = None;
            }
        }
    }

    /// Wait for any in-flight autosave, then delete the autosave file.
    ///
    /// Ordering matters. Deleting first would let a still-running write
    /// recreate the file immediately afterwards, leaving a stale autosave that
    /// prompts for recovery on the next launch — offering to "restore" edits
    /// the user already saved.
    fn clear_autosave(&mut self) {
        if let Some(rx) = self.autosave_rx.take() {
            // Bounded: the worker only serializes an overlay, and a lost
            // sender resolves immediately as Disconnected.
            let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
        }
        if let Some(path) = &self.edits_path {
            if let Err(e) = ferrix_io::edits::discard_autosave(path) {
                self.status = format!("Could not remove autosave: {e}");
            }
        }
        self.autosave_revision = None;
    }

    /// The recovery prompt shown when a previous session left an autosave
    /// newer than the sidecar — i.e. it did not exit cleanly.
    ///
    /// Two options, and neither is silent. Recover loads the autosaved overlay
    /// and leaves the workbook dirty, so the user still owns the decision to
    /// commit it. Discard deletes the autosave and touches nothing else — the
    /// official sidecar is not modified either way.
    fn show_recovery_prompt(&mut self, ctx: &egui::Context) {
        let Some(candidate) = self.recovery.clone() else {
            return;
        };
        let th = self.theme;
        let mut recover = false;
        let mut discard = false;

        egui::Window::new("Recover unsaved edits")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("Recover edits from {} ago?", candidate.age_hhmm()))
                        .size(13.5),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Ferrix did not shut down cleanly. These edits were saved \
                         automatically and are not in the saved file yet.",
                    )
                    .color(th.text_dim)
                    .size(11.5),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Recover").clicked() {
                        recover = true;
                    }
                    if ui.button("Discard").clicked() {
                        discard = true;
                    }
                });
            });

        if recover {
            self.recover_autosave();
        } else if discard {
            self.discard_recovery();
        }
    }

    /// Load the autosaved overlay into the workbook.
    ///
    /// The overlay replaces what is on screen and the workbook is marked
    /// dirty: recovered edits are unsaved by definition, and the Save button
    /// must reflect that. The autosave file is left in place until the user
    /// saves or exits cleanly, so a crash during recovery does not lose it.
    pub fn recover_autosave(&mut self) {
        self.recovery = None;
        self.recovery_resolved = true;
        let (Some(path), Some(fp)) = (self.edits_path.clone(), self.fingerprint) else {
            return;
        };
        match ferrix_io::edits::load_autosave(&path, fp) {
            Ok(Some(ov)) => {
                let n = ov.len();
                self.wb.adopt_overlay(ov);
                // Formula sources were saved with a cached result computed
                // against the base at autosave time; re-evaluate so what the
                // user sees matches the data actually in front of them.
                self.wb.rebuild_graph_and_recalc();
                self.wb.mark_dirty();
                // A recovered overlay is a new starting point for the timer.
                self.autosave_revision = None;
                self.autosave_last = Some(std::time::Instant::now());
                self.sync_formula_bar();
                self.status = format!("Recovered {} unsaved edit{}", fmt_int(n), plural(n));
            }
            Ok(None) => {
                self.status = "Autosave vanished before it could be recovered".into();
            }
            Err(e) => {
                // Typically a stale base. Say so rather than applying edits by
                // position onto data that has changed underneath them.
                self.status = format!("Could not recover autosaved edits — {e}");
            }
        }
    }

    /// Throw the autosave away, leaving the official sidecar untouched.
    pub fn discard_recovery(&mut self) {
        self.recovery = None;
        self.recovery_resolved = true;
        if let Some(path) = self.edits_path.clone() {
            match ferrix_io::edits::discard_autosave(&path) {
                Ok(()) => self.status = "Discarded autosaved edits".into(),
                Err(e) => self.status = format!("Could not discard autosave: {e}"),
            }
        }
        self.autosave_revision = None;
    }

    /// Whether the recovery prompt is currently on screen.
    pub fn recovery_prompt_open(&self) -> bool {
        self.recovery.is_some()
    }

    /// The prompt's headline text, for tests and for the status bar.
    pub fn recovery_prompt_text(&self) -> Option<String> {
        self.recovery
            .as_ref()
            .map(|c| format!("Recover edits from {} ago?", c.age_hhmm()))
    }

    /// Test/afterlife hook: does an autosave file exist for the open dataset?
    pub fn autosave_file_exists(&self) -> bool {
        self.edits_path
            .as_ref()
            .map(|p| ferrix_io::edits::autosave_path_for_sidecar(p).exists())
            .unwrap_or(false)
    }

    /// Force an autosave tick regardless of the wall clock, for tests.
    ///
    /// Waiting 30 real seconds in a test would be intolerable, and shortening
    /// the interval only to sleep is the same thing with extra steps. This
    /// drives the SAME path the timer drives — including the no-change skip —
    /// by moving the deadline into the past rather than bypassing the logic.
    pub fn autosave_tick_now(&mut self) {
        if let Some(last) = self.autosave_last.as_mut() {
            *last -= self.prefs.autosave_interval() + std::time::Duration::from_millis(1);
        } else {
            self.autosave_last = Some(
                std::time::Instant::now()
                    - self.prefs.autosave_interval()
                    - std::time::Duration::from_millis(1),
            );
        }
        self.tick_autosave();
        // Autosave is asynchronous by design; a test wants the outcome.
        self.wait_for_autosave();
    }

    /// Block until any in-flight autosave has landed. Test-facing.
    pub fn wait_for_autosave(&mut self) {
        if let Some(rx) = self.autosave_rx.take() {
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Ok((_bytes, revision))) => self.autosave_revision = Some(revision),
                Ok(Err(e)) => self.status = format!("Autosave failed: {e}"),
                Err(_) => {}
            }
        }
    }

    /// Set the autosave cadence for this session. Test-facing.
    pub fn set_autosave_secs(&mut self, secs: u64) {
        self.prefs.autosave_secs = Some(secs);
    }

    /// Shut down cleanly: no autosave survives a deliberate exit.
    ///
    /// The autosave file exists to answer "did we crash?". A file left behind
    /// by an orderly shutdown would answer yes, and the next launch would
    /// offer to recover edits from a session that ended perfectly normally.
    pub fn on_clean_exit(&mut self) {
        // Where the user was is part of a clean exit: reopening this file
        // must land them back here (issue #45).
        self.persist_session();
        self.clear_autosave();
    }

    // --- Compact (roadmap #7) ---

    /// Can this sheet be compacted right now?
    ///
    /// Requires a columnar cache (the in-RAM and xlsx paths have none), edits
    /// worth baking, and no other long job in flight — two writers over the
    /// same file would race for the rename.
    fn can_compact(&self) -> bool {
        self.cache_path.is_some()
            && !self.wb.overlay.is_empty()
            && !self.compacting
            && !self.loading
            && !self.exporting
    }

    /// Why Compact is unavailable, in the user's terms. A greyed-out menu item
    /// with no explanation is indistinguishable from a bug.
    fn compact_hint(&self) -> String {
        if self.cache_path.is_none() {
            "Only files large enough to use the columnar cache can be compacted".to_string()
        } else if self.wb.overlay.is_empty() {
            "Nothing to compact — there are no edits to bake in".to_string()
        } else if self.compacting {
            "A compact is already running".to_string()
        } else if self.loading || self.exporting {
            "Wait for the current operation to finish".to_string()
        } else {
            format!(
                "Rewrite the cache with {} edit{} baked in, then retire the sidecar",
                fmt_int(self.wb.overlay.len()),
                if self.wb.overlay.len() == 1 { "" } else { "s" }
            )
        }
    }

    /// Start a compact on a worker thread.
    ///
    /// ## Why the mapping is dropped first
    ///
    /// The compactor renames the new cache over the old one. On Windows an
    /// open memory mapping locks the file and the rename fails; on Unix it
    /// would succeed but leave this process reading a deleted inode — stale
    /// data with no warning. Either way the live mapping must go before the
    /// commit. So the base is swapped for an empty in-RAM sheet for the
    /// duration, the modal blocks interaction, and the new mapping is adopted
    /// when the worker reports back.
    ///
    /// The overlay is *not* dropped: if the compact fails or is cancelled, the
    /// original cache is still on disk and re-mapping it restores exactly the
    /// state the user was in.
    pub fn start_compact(&mut self) {
        if !self.can_compact() {
            self.status = self.compact_hint();
            return;
        }
        let Some(cache) = self.cache_path.clone() else {
            return;
        };

        // The overlay is cloned for the worker so the UI keeps its copy to
        // restore from on failure. Cost is O(edits) — the same snapshot the
        // export path already admits against the budget.
        let overlay = self.wb.overlay.clone();
        let edits = overlay.len();

        // Let go of the mapping before anything can try to rename over it.
        self.wb
            .replace_base(BaseData::Memory(ferrix_core::Sheet::new("compacting")));

        let (tx, rx) = channel::<Result<CompactDone, String>>();
        let (ptx, prx) = channel::<Progress>();
        let cancel = ferrix_core::CancelToken::new();
        let mut should_cancel = cancel.checker();
        let target = cache.clone();

        std::thread::spawn(move || {
            let result = ferrix_io::compact::compact_cache(
                &target,
                &overlay,
                |done, total| {
                    let _ = ptx.send(Progress { done, total });
                },
                &mut should_cancel,
            )
            .map_err(|e| e.to_string())
            .map(|out| CompactDone {
                rows: out.stats.rows,
                cols: out.stats.cols,
                edits_baked: out.stats.edits_baked,
                formulas_kept: out.stats.formulas_kept,
                output_bytes: out.stats.output_bytes,
                millis: out.stats.millis,
                peak_heap_bytes: out.stats.peak_heap_bytes(),
                residual: out.residual,
                sidecar: out.sidecar,
            });
            let _ = tx.send(result);
        });

        self.compacting = true;
        self.compact_cancel = Some(cancel);
        self.compact_rx = Some(rx);
        self.compact_progress_rx = Some(prx);
        self.compact_progress = Progress::default();
        self.compact_started = Some(std::time::Instant::now());
        self.status = format!("Compacting {} edits into the cache…", fmt_int(edits));
    }

    /// Whether the Compact menu item is enabled, for the headless harness.
    pub fn compact_available(&self) -> bool {
        self.can_compact()
    }

    /// The Compact tooltip, for the headless harness.
    pub fn compact_tooltip(&self) -> String {
        self.compact_hint()
    }

    /// Is a compact in flight? For the headless harness.
    pub fn is_compacting(&self) -> bool {
        self.compacting
    }

    /// Attach an existing `.ferrix` cache to this app as the active base.
    ///
    /// Test-only. The real path only reaches the mmap branch for files above
    /// 1 GB, which no test should be writing, so this is how a harness gets a
    /// mapped sheet with a known cache under it.
    #[cfg(test)]
    pub fn adopt_cache_for_test(&mut self, cache: &Path) -> Result<(), String> {
        self.cache_path = Some(cache.to_path_buf());
        let (rows, cols) = self.remap_cache()?;
        self.stats_rows = rows;
        self.stats_cols = cols;
        self.edits_path = Some(ferrix_io::edits::edits_path_for(cache));
        self.fingerprint =
            ferrix_io::compact::fingerprint_after(cache, rows as u64, cols as u32).ok();
        // A cache is now open, so the start screen must step aside — it takes
        // the whole frame and the grid would never run behind it (issue #45).
        self.source_path = Some(cache.to_path_buf());
        self.show_start = false;
        self.workbook_started = true;
        Ok(())
    }

    /// The active sidecar path, for the headless harness.
    pub fn sidecar_path(&self) -> Option<&Path> {
        self.edits_path.as_deref()
    }

    /// Ask a running compact to stop.
    ///
    /// The worker polls between columns and every 64K rows, deletes its
    /// scratch file, and returns before the rename — so the original cache and
    /// sidecar are both untouched.
    pub fn cancel_compact(&mut self) {
        if let Some(c) = &self.compact_cancel {
            c.cancel();
            self.status = "Cancelling compact…".into();
        }
    }

    /// Re-map the cache and adopt it as the active base.
    ///
    /// Used on both the success and failure paths: on success it picks up the
    /// rewritten file, on failure the original, which is still exactly where
    /// it was.
    fn remap_cache(&mut self) -> Result<(usize, usize), String> {
        let Some(cache) = self.cache_path.clone() else {
            return Err("no cache to map".into());
        };
        let mut mapped = ferrix_io::MappedSheet::open(&cache).map_err(|e| e.to_string())?;
        if !self.cache_headers.is_empty() {
            mapped.set_headers(self.cache_headers.clone());
        }
        let (rows, cols) = (mapped.row_count(), mapped.col_count());
        self.wb.replace_base(BaseData::Mapped(Box::new(mapped)));
        Ok((rows, cols))
    }

    /// Drain compact progress and completion.
    fn poll_compact(&mut self) {
        if let Some(prx) = &self.compact_progress_rx {
            while let Ok(p) = prx.try_recv() {
                self.compact_progress = p;
            }
        }

        let Some(rx) = &self.compact_rx else { return };
        let outcome = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("Compact thread died — the original cache and edits are unchanged".to_string())
            }
        };

        self.compacting = false;
        self.compact_rx = None;
        self.compact_progress_rx = None;
        self.compact_cancel = None;
        let secs = self
            .compact_started
            .take()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);

        match outcome {
            Ok(done) => {
                // The overlay is now IN the cache. What remains is whatever
                // could not be baked — formulas, which keep their source so
                // they can be re-evaluated against the new base.
                self.wb.adopt_overlay(done.residual);
                self.edits_path = done.sidecar.clone();

                match self.remap_cache() {
                    Ok((rows, cols)) => {
                        self.stats_rows = rows;
                        self.stats_cols = cols;
                        self.col_widths = match &*self.wb.base {
                            BaseData::Mapped(m) => compute_col_widths_mapped(m),
                            BaseData::Memory(s) => compute_col_widths_mem(s),
                        };
                        // Re-anchor the fingerprint: the base has changed, so
                        // every fingerprint taken against the old one is stale
                        // by construction and a later save would be rejected.
                        let cache = self.cache_path.clone();
                        self.fingerprint = cache.as_ref().and_then(|c| {
                            ferrix_io::compact::fingerprint_after(c, rows as u64, cols as u32).ok()
                        });
                        if self.edits_path.is_none() {
                            // No residual sidecar was written, but a future
                            // edit still needs somewhere to go.
                            self.edits_path =
                                cache.as_ref().map(|c| ferrix_io::edits::edits_path_for(c));
                        }
                    }
                    Err(e) => {
                        self.status = format!(
                            "Compacted, but the new cache could not be opened: {e} — reopen the file"
                        );
                        return;
                    }
                }

                // Undo history does not survive a compact, for the same reason
                // it does not survive a save, only more so: the timeline's
                // "before" states describe a file that no longer exists.
                let lost = self.wb.save_committed();
                self.autosave_last = None;
                self.autosave_revision = None;
                let history = if lost > 0 {
                    format!(
                        " · undo history cleared ({} step{})",
                        fmt_int(lost),
                        if lost == 1 { "" } else { "s" }
                    )
                } else {
                    String::new()
                };
                let kept = if done.formulas_kept > 0 {
                    format!(
                        " · {} formula{} kept in a new sidecar",
                        fmt_int(done.formulas_kept),
                        if done.formulas_kept == 1 { "" } else { "s" }
                    )
                } else {
                    " · sidecar retired".to_string()
                };
                self.status = format!(
                    "Compacted {} edits into {} rows × {} cols ({:.1} GB) in {:.1}s · peak {:.0} MB{}{}",
                    fmt_int(done.edits_baked),
                    fmt_int(done.rows as usize),
                    done.cols,
                    done.output_bytes as f64 / 1e9,
                    if done.millis > 0 {
                        done.millis as f64 / 1000.0
                    } else {
                        secs
                    },
                    done.peak_heap_bytes as f64 / 1e6,
                    kept,
                    history
                );
            }
            Err(e) => {
                // Nothing on disk changed. Put the original mapping back so
                // the user is exactly where they were, edits and all.
                let restored = self.remap_cache();
                self.status = if e.contains("cancelled") {
                    "Compact cancelled — the cache and your edits are unchanged".to_string()
                } else {
                    format!("Compact failed: {e} — the cache and your edits are unchanged")
                };
                if let Err(e2) = restored {
                    self.status = format!("{} (could not re-open the cache: {e2})", self.status);
                }
            }
        }
    }

    /// The modal shown while a compact runs.
    ///
    /// Deliberately modal rather than a toolbar spinner like export: an export
    /// only reads, so editing during one is harmless, while a compact is
    /// rewriting the very file the grid reads from. Blocking input is the
    /// simplest honest way to say "this file is being replaced".
    fn show_compact_modal(&mut self, ctx: &egui::Context) {
        let th = self.theme;
        let mut cancel = false;
        let p = self.compact_progress;

        egui::Window::new("Compacting")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(
                    RichText::new("Baking edits into the columnar cache")
                        .color(th.text)
                        .size(14.0),
                );
                ui.add_space(6.0);
                if p.total > 0 {
                    let frac = p.done as f32 / p.total as f32;
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(340.0)
                            .text(format!("column {} of {}", p.done, p.total)),
                    );
                } else {
                    ui.add(
                        egui::ProgressBar::new(0.0)
                            .desired_width(340.0)
                            .text("starting…"),
                    );
                }
                ui.add_space(6.0);
                // Say what is and is not at risk. A progress bar over someone's
                // data file without this sentence is just anxiety.
                ui.label(
                    RichText::new(
                        "Your existing file is untouched until this finishes. \
                         Cancelling now changes nothing.",
                    )
                    .color(th.text_dim)
                    .size(12.0),
                );
                ui.add_space(8.0);
                if ui.button("✖ Cancel").clicked() {
                    cancel = true;
                }
            });

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            cancel = true;
        }
        if cancel {
            self.cancel_compact();
        }
    }

    /// The unsaved-changes modal shown when the user tries to close the window
    /// with a dirty workbook.
    ///
    /// Three honest options: Save (write the sidecar, then close), Discard
    /// (close and lose the edits — stated plainly), and Cancel (stay put).
    /// The cell context menu: right-click on a cell.
    ///
    /// A free-floating window rather than egui's `Response::context_menu`,
    /// because the grid paints onto a raw `Painter` and allocates no per-cell
    /// widget to hang a menu off — the whole reason a 200M-row sheet paints in
    /// constant time.
    /// Open the right-click cell menu anchored at `pos`. Right-clicking a cell
    /// OUTSIDE the current selection moves the selection to it first, so the
    /// menu's actions operate on what the user pointed at (matching Excel);
    /// right-clicking inside a multi-cell selection leaves it intact so block
    /// operations still work. Public so the harness can exercise the reselect
    /// rule without hit-testing a floating popup.
    pub fn open_cell_menu(&mut self, cell: CellRef, pos: egui::Pos2) {
        if !self.selection.contains(cell) {
            self.selection = ferrix_core::Selection::new(cell, cell);
        }
        self.context_cell = Some(cell);
        self.context_pos = pos;
    }

    /// True while the right-click cell menu is open, for tests.
    pub fn cell_menu_open(&self) -> bool {
        self.context_cell.is_some()
    }

    fn show_cell_menu(&mut self, ctx: &egui::Context) {
        let Some(cell) = self.context_cell else {
            return;
        };
        let has_comment = self.wb.comments.contains(cell);
        let multi = !self.selection.is_single();

        // Actions collected from the menu, applied after the closure so the
        // borrow of `self` inside the Area is released first.
        let mut do_copy = false;
        let mut do_cut = false;
        let mut do_clear = false;
        let mut do_insert_row = false;
        let mut do_delete_row = false;
        let mut do_insert_col = false;
        let mut do_delete_col = false;
        let mut do_set_print_area = false;
        let mut insert_comment = false;
        let mut delete_comment = false;
        let mut close = false;

        let resp = egui::Area::new(egui::Id::new("ferrix_cell_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(self.context_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(190.0);
                    let heading = if multi {
                        format!("{} selected", cell_label(cell))
                    } else {
                        cell_label(cell)
                    };
                    ui.label(RichText::new(heading).weak().size(11.0));
                    ui.separator();

                    // Clipboard. Paste is intentionally absent: egui exposes no
                    // clipboard-read API (it only delivers a Paste event on
                    // Ctrl+V), so a menu item could not read the clipboard. The
                    // item would have to lie or do nothing; Ctrl+V stays the way
                    // to paste and the menu says so.
                    if ui.button("Copy").clicked() {
                        do_copy = true;
                    }
                    if ui.button("Cut").clicked() {
                        do_cut = true;
                    }
                    if ui.button("Clear Contents").clicked() {
                        do_clear = true;
                    }
                    ui.add_enabled(false, egui::Button::new("Paste  (Ctrl+V)"));
                    ui.separator();

                    // Structure.
                    if ui.button("Insert row(s)").clicked() {
                        do_insert_row = true;
                    }
                    if ui.button("Delete row(s)").clicked() {
                        do_delete_row = true;
                    }
                    if ui.button("Insert column(s)").clicked() {
                        do_insert_col = true;
                    }
                    if ui.button("Delete column(s)").clicked() {
                        do_delete_col = true;
                    }
                    ui.separator();

                    // Print / comment.
                    if ui.button("Set Print Area").clicked() {
                        do_set_print_area = true;
                    }
                    let comment_label = if has_comment {
                        "Edit Comment…"
                    } else {
                        "Insert Comment…"
                    };
                    if ui.button(comment_label).clicked() {
                        insert_comment = true;
                    }
                    if ui
                        .add_enabled(has_comment, egui::Button::new("Delete Comment"))
                        .clicked()
                    {
                        delete_comment = true;
                    }
                });
            });

        // Dismiss on Escape, or on a click anywhere outside the menu. Without
        // the second the menu would be modal in practice.
        let clicked_outside = ctx.input(|i| i.pointer.any_click())
            && !resp.response.rect.contains(
                ctx.input(|i| i.pointer.interact_pos())
                    .unwrap_or(egui::Pos2::ZERO),
            );
        if ctx.input(|i| i.key_pressed(Key::Escape)) || clicked_outside {
            close = true;
        }

        // Apply. Any chosen action closes the menu.
        if do_copy {
            self.copy_selection(ctx, false);
            close = true;
        }
        if do_cut {
            self.copy_selection(ctx, true);
            close = true;
        }
        if do_clear {
            self.clear_selection();
            close = true;
        }
        if do_insert_row {
            self.run_command(crate::command::CommandId::DataInsertRow);
            close = true;
        }
        if do_delete_row {
            self.run_command(crate::command::CommandId::DataDeleteRow);
            close = true;
        }
        if do_insert_col {
            self.run_command(crate::command::CommandId::DataInsertColumn);
            close = true;
        }
        if do_delete_col {
            self.run_command(crate::command::CommandId::DataDeleteColumn);
            close = true;
        }
        if do_set_print_area {
            self.run_command(crate::command::CommandId::FileSetPrintArea);
            close = true;
        }
        if insert_comment {
            self.begin_comment(cell);
            close = true;
        }
        if delete_comment {
            self.delete_comment(cell);
            close = true;
        }
        if close {
            self.context_cell = None;
        }
    }

    /// The comment editor: a small modal over the grid.
    fn show_comment_editor(&mut self, ctx: &egui::Context) {
        let Some(cell) = self.comment_editing else {
            return;
        };
        let mut commit = false;
        let mut cancel = false;
        let mut delete = false;
        let had_one = self.wb.comments.contains(cell);

        egui::Window::new(format!("Comment on {}", cell_label(cell)))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Author");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.comment_author_buffer)
                            .desired_width(180.0),
                    );
                });
                ui.add_space(4.0);
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut self.comment_buffer)
                        .desired_width(320.0)
                        .desired_rows(4),
                );
                if self.comment_focus_pending {
                    resp.request_focus();
                    self.comment_focus_pending = false;
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if ui
                        .add_enabled(had_one, egui::Button::new("Delete"))
                        .clicked()
                    {
                        delete = true;
                    }
                });
                // Ctrl+Enter commits; a bare Enter has to insert a newline,
                // because a note is prose and multi-line notes are the point.
                if ui.input(|i| i.key_pressed(Key::Enter) && i.modifiers.command) {
                    commit = true;
                }
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel = true;
                }
            });

        if delete {
            self.comment_editing = None;
            self.comment_focus_pending = false;
            self.comment_buffer.clear();
            self.delete_comment(cell);
        } else if commit {
            self.commit_comment();
        } else if cancel {
            self.cancel_comment();
        }
    }

    /// The Goal Seek dialog (issue #35).
    ///
    /// Records the real painted rects of Solve and Cancel into the state, so
    /// Draw the import wizard and act on Import / Cancel (issue #31).
    fn show_import_wizard(&mut self, ctx: &egui::Context) {
        let Some(mut w) = self.import_wizard.take() else {
            return;
        };
        crate::import_wizard::show(&mut w, ctx, &self.theme);
        if w.accepted {
            self.accept_import(w);
        } else if w.cancelled {
            // Cancelling opens nothing. The previously open file, if any, is
            // untouched — this is the same contract as a failed load.
            self.status = format!("Import cancelled — {} was not opened", w.path.display());
            self.pending_path = None;
            self.show_start = self.source_path.is_none();
        } else {
            self.import_wizard = Some(w);
        }
    }

    /// Apply the wizard's settings: remember them if asked, then load.
    fn accept_import(&mut self, w: crate::import_wizard::ImportWizard) {
        if w.remember {
            let o = w.options();
            self.prefs.set_import_rule(crate::prefs::ImportRule {
                name: w.remember_key(),
                encoding: o.encoding.map(|e| e.name().to_string()).unwrap_or_default(),
                delimiter: o.delimiter,
                quote: o.quote,
                has_headers: o.has_headers,
                skip_rows: o.skip_rows,
            });
            self.persist_prefs();
        } else {
            // Unchecking the box on a file that HAD a rule means "stop
            // remembering". Leaving the old rule in place would make the
            // checkbox look like it did nothing, and the next open would
            // silently use settings the user just declined.
            if self.prefs.clear_import_rule(&w.remember_key()) {
                self.persist_prefs();
            }
        }
        self.start_load_with(w.path.clone(), w.options());
    }

    /// Open the wizard for the file already loaded, so settings can be
    /// revisited without reopening from the menu.
    pub fn reopen_import_wizard(&mut self) {
        let Some(path) = self.source_path.clone() else {
            self.status = "No file open to re-import".into();
            return;
        };
        match crate::import_wizard::ImportWizard::for_path(&path) {
            Ok(w) => self.import_wizard = Some(w),
            Err(e) => self.status = format!("Cannot read {}: {e}", path.display()),
        }
    }

    /// The wizard, for tests that need to drive it without synthetic input.
    pub fn import_wizard(&self) -> Option<&crate::import_wizard::ImportWizard> {
        self.import_wizard.as_ref()
    }

    pub fn import_wizard_mut(&mut self) -> Option<&mut crate::import_wizard::ImportWizard> {
        self.import_wizard.as_mut()
    }

    pub fn import_wizard_is_open(&self) -> bool {
        self.import_wizard.is_some()
    }

    /// Confirm the wizard from a test or a command, exactly as the Import
    /// button does.
    pub fn import_wizard_accept(&mut self) {
        if let Some(w) = self.import_wizard.take() {
            self.accept_import(w);
        }
    }

    /// The Goal Seek dialog (issue #35).
    ///
    /// Records the real painted rects of Solve and Cancel into the state, so
    /// harness tests click the widget that was actually drawn rather than
    /// calling the handler behind it — a dialog whose button never paints
    /// fails the test instead of passing it.
    fn show_goal_seek(&mut self, ctx: &egui::Context) {
        if self.goal_seek.is_none() {
            return;
        }
        let th = self.theme;
        let mut solve = false;
        let mut cancel = false;
        let mut close = false;
        // Taken out so the closure can hold `&mut` on the fields without
        // borrowing all of `self`; put back immediately after.
        let mut st = self.goal_seek.take().expect("checked above");

        egui::Window::new("Goal Seek")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("goal_seek_grid")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Set cell");
                        ui.add(
                            egui::TextEdit::singleline(&mut st.set_cell)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("C10")
                                .desired_width(120.0),
                        );
                        ui.end_row();

                        ui.label("To value");
                        ui.add(
                            egui::TextEdit::singleline(&mut st.to_value)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("0")
                                .desired_width(120.0),
                        );
                        ui.end_row();

                        ui.label("By changing cell");
                        ui.add(
                            egui::TextEdit::singleline(&mut st.by_changing)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("B4")
                                .desired_width(120.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let r = ui.button("Solve");
                    st.solve_rect = Some(r.rect);
                    if r.clicked() {
                        solve = true;
                    }
                    // Cancel means "put it back", which is only distinct from
                    // Close once a run has applied something. Labelled so the
                    // difference is visible rather than inferred.
                    let c = ui
                        .button(if st.applied {
                            "Cancel (restore)"
                        } else {
                            "Cancel"
                        })
                        .on_hover_text(
                            "Undoes the whole Goal Seek run in one step, restoring the \
                             changing cell.",
                        );
                    st.cancel_rect = Some(c.rect);
                    if c.clicked() {
                        cancel = true;
                    }
                    if st.applied && ui.button("Keep").clicked() {
                        close = true;
                    }
                });

                if let Some((msg, is_err)) = &st.message {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.label(
                        RichText::new(msg)
                            .color(if *is_err { th.error } else { th.number })
                            .size(12.5),
                    );
                }

                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel = true;
                }
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    solve = true;
                }
            });

        self.goal_seek = Some(st);
        if solve {
            self.goal_seek_solve();
        } else if cancel {
            self.goal_seek_cancel();
        } else if close {
            self.goal_seek_close();
        }
    }

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
            // A deliberate discard is still a clean exit: the user chose to
            // drop these edits, so leaving an autosave that resurrects them
            // on the next launch would override that choice.
            self.clear_autosave();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The overwrite confirmation for a drag-drop that would clobber non-empty
    /// cells (#82). Excel shows the same "There's already data here." prompt.
    ///
    /// Replace carries out the parked move/copy; Cancel drops it and leaves
    /// every cell untouched. Escape and clicking away both cancel, matching the
    /// rest of the app's dialogs — the safe default when the answer would
    /// destroy data.
    fn show_block_move_prompt(&mut self, ctx: &egui::Context) {
        let th = self.theme;
        let mut replace = false;
        let mut cancel = false;
        let copy = self.pending_block_move.map(|p| p.copy).unwrap_or(false);
        let verb = if copy { "copied" } else { "moved" };

        egui::Window::new("Replace existing data?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("There's already data here.")
                        .size(13.5)
                        .strong(),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!(
                        "The cells being {verb} would overwrite existing values."
                    ))
                    .color(th.text_dim)
                    .size(11.5),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Replace").clicked() {
                        replace = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        // Escape cancels, matching every other dialog in the app.
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            cancel = true;
        }

        if replace {
            self.confirm_block_move();
        } else if cancel {
            self.cancel_block_move();
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

    // ------------------------------------------------ cell comments (#12)

    /// Open the comment editor on `cell`, seeded with any existing note.
    ///
    /// One entry point for both "insert" and "edit": the two differ only in
    /// whether the buffer starts empty, and keeping them one function means a
    /// change to how comments are written cannot apply to only one of them.
    pub fn begin_comment(&mut self, cell: CellRef) {
        // A merge-covered cell is not a cell the user can point at
        // independently — its value lives on the anchor. Route the note there
        // too, or a comment would attach to something invisible.
        let cell = self.wb.merges.resolve(cell);
        let existing = self.wb.comments.get(cell);
        self.comment_buffer = existing.map(|c| c.text.clone()).unwrap_or_default();
        self.comment_author_buffer = existing
            .map(|c| c.author.clone())
            .unwrap_or_else(default_comment_author);
        self.comment_editing = Some(cell);
        self.comment_focus_pending = true;
    }

    /// Commit the open comment editor.
    ///
    /// Empty text DELETES the comment rather than storing a blank one: a
    /// marker triangle promising a note that turns out to be empty is worse
    /// than no marker, and "clear the box" is how a user expects to remove one.
    pub fn commit_comment(&mut self) {
        let Some(cell) = self.comment_editing.take() else {
            return;
        };
        self.comment_focus_pending = false;
        let text = std::mem::take(&mut self.comment_buffer);
        let author = std::mem::take(&mut self.comment_author_buffer);
        if text.trim().is_empty() {
            if self.wb.comments.remove(cell).is_some() {
                self.wb.mark_dirty();
                self.status = format!("Removed comment on {}", cell_label(cell));
            }
            return;
        }
        let existed = self
            .wb
            .comments
            .set(cell, ferrix_core::Comment::new(author, text))
            .is_some();
        self.wb.mark_dirty();
        self.status = format!(
            "{} comment on {}",
            if existed { "Updated" } else { "Added" },
            cell_label(cell)
        );
    }

    /// Fill the open comment editor's buffers, as typing into them would.
    ///
    /// Exists so a test can exercise begin/commit without synthesising
    /// keystrokes into a text box whose position depends on the theme.
    pub fn set_comment_buffers_for_test(&mut self, author: &str, text: &str) {
        self.comment_author_buffer = author.to_string();
        self.comment_buffer = text.to_string();
    }

    /// Abandon the open comment editor, changing nothing.
    pub fn cancel_comment(&mut self) {
        self.comment_editing = None;
        self.comment_focus_pending = false;
        self.comment_buffer.clear();
        self.comment_author_buffer.clear();
    }

    /// Delete a cell's comment. Reports when there was nothing to delete
    /// rather than silently doing nothing.
    pub fn delete_comment(&mut self, cell: CellRef) {
        let cell = self.wb.merges.resolve(cell);
        match self.wb.comments.remove(cell) {
            Some(_) => {
                self.wb.mark_dirty();
                self.status = format!("Removed comment on {}", cell_label(cell));
            }
            None => self.status = format!("{} has no comment", cell_label(cell)),
        }
    }

    /// Whether a comment editor is currently open — used by the key handler to
    /// keep grid navigation out of the text box.
    pub fn comment_editor_open(&self) -> bool {
        self.comment_editing.is_some()
    }

    /// Comment count on the active sheet, for tests and the status bar.
    pub fn comment_count(&self) -> usize {
        self.wb.comments.len()
    }

    /// A cell's comment text, for tests.
    pub fn comment_text(&self, cell: CellRef) -> Option<&str> {
        self.wb.comments.get(cell).map(|c| c.text.as_str())
    }

    /// The live comment map, for the paint-cost assertions.
    pub fn comment_map(&self) -> &ferrix_core::CommentMap {
        &self.wb.comments
    }

    /// Cells painted by the last frame.
    pub fn painted_cell_count(&self) -> usize {
        self.last_painted
    }

    /// Screen rows painted by the last frame.
    pub fn painted_row_count(&self) -> usize {
        self.last_painted_rows.len()
    }

    /// Comment markers actually PAINTED by the last frame.
    ///
    /// Read from the grid's paint output rather than from the map, so a test
    /// asserting a deleted comment's marker is gone is reading the screen.
    pub fn painted_comment_markers(&self) -> usize {
        self.last_comment_markers
    }

    // --- painted decoration counters (issue #28) ---
    //
    // All three come from the grid's paint output, so they answer "did this
    // actually get drawn" rather than "is it in the model". A model-only
    // assertion would pass against a perfectly-stored, never-painted format —
    // which is precisely how four earlier features in this repo shipped
    // unreachable.

    /// Border edges painted last frame, counted once per EDGE.
    pub fn painted_border_segments(&self) -> usize {
        self.last_border_segments
    }

    /// Cells whose text was painted rotated last frame.
    pub fn painted_rotated_texts(&self) -> usize {
        self.last_rotated_texts
    }

    /// Cells whose text was laid out wrapped last frame.
    pub fn painted_wrapped_texts(&self) -> usize {
        self.last_wrapped_texts
    }

    /// Sparkline primitives painted last frame (issue #36).
    ///
    /// Zero on every sheet with no sparkline group. This is the number a test
    /// asserts on rather than `paint_shape_count()`: a frame total moves when
    /// a selection rectangle appears or a grid line leaves, so it can rise
    /// while the feature draws nothing and fall while it draws plenty.
    pub fn painted_sparklines(&self) -> usize {
        self.last_sparkline_shapes
    }

    /// Cells a sparkline group covers that deliberately drew NOTHING last
    /// frame, because their source was empty or held no numbers.
    pub fn blank_sparklines(&self) -> usize {
        self.last_sparkline_blanks
    }

    /// Persist comments beside the base file.
    ///
    /// Independent of `save_edits`: a session that only added a note has
    /// nothing for the edits sidecar to write, and must still not lose the
    /// note.
    pub fn save_comments(&mut self) -> bool {
        let Some(path) = self.comments_path.clone() else {
            return false;
        };
        ferrix_io::save_comments(&path, &self.wb.comments).is_ok()
    }

    /// Persist the sizing sidecar beside the base file (issue #29).
    ///
    /// Written from `save_edits` unconditionally, like comments: a session
    /// that only resized a column has an EMPTY overlay, and the edits path
    /// returns early on that — so gating sizing behind it would silently
    /// discard exactly the work this feature exists to keep.
    pub fn save_sizing(&mut self) -> bool {
        let Some(path) = self.sizing_path.clone() else {
            return false;
        };
        // An empty sizing state writes an empty sidecar rather than skipping:
        // the user may have RESET a width back to default, and leaving the old
        // file in place would restore a size they explicitly removed.
        let ok = ferrix_io::save_sizing(&path, &self.sizing).is_ok();
        if ok {
            self.sizing_dirty = false;
        }
        ok
    }

    /// Load the sizing sidecar for the file that was just opened.
    pub fn load_sizing_sidecar(&mut self) {
        let Some(path) = self.sizing_path.clone() else {
            return;
        };
        // A corrupt or unreadable sidecar leaves sizing at its defaults rather
        // than failing the whole load: the user's DATA is fine, and refusing
        // to open a 10GB file because a layout file is damaged would be a poor
        // trade.
        if let Ok(Some(s)) = ferrix_io::load_sizing(&path) {
            self.set_sizing(s);
            self.sizing_dirty = false;
        }
    }

    /// Persist the pivot sidecar beside the base file (issue #33 Part B).
    ///
    /// Written from `save_edits` unconditionally, like comments and sizing: a
    /// session that only defined a pivot has an EMPTY overlay, and the edits
    /// path returns early on that — so gating pivots behind it would silently
    /// discard exactly the binding this feature exists to keep. Exporting the
    /// current bindings and saving an empty list retires the sidecar when the
    /// last pivot is cleared, so a removed pivot does not come back on reload.
    pub fn save_pivots(&mut self) -> bool {
        let Some(path) = self.pivots_path.clone() else {
            return false;
        };
        let records = self.wb.export_pivots();
        ferrix_io::save_pivots(&path, &records).is_ok()
    }

    /// Load the pivot sidecar for the file that was just opened, then refresh
    /// each pivot so its cells show a computed result immediately.
    ///
    /// A corrupt or unreadable sidecar leaves the workbook with no pivots rather
    /// than failing the whole load — the same "the DATA is fine" stance the
    /// sizing and comment loaders take. Refreshing here (not lazily) means the
    /// first painted frame already has the pivot's values.
    pub fn load_pivots_sidecar(&mut self) {
        let Some(path) = self.pivots_path.clone() else {
            return;
        };
        if let Ok(Some(records)) = ferrix_io::load_pivots(&path) {
            self.wb.adopt_pivots(&records);
            // Populate each pivot's cached result from its restored spec.
            let sheets: Vec<_> = self
                .wb
                .sheet_names()
                .iter()
                .map(|(id, _)| *id)
                .filter(|&id| self.wb.is_pivot_sheet(id))
                .collect();
            for id in sheets {
                self.wb.refresh_pivot(id);
            }
        }
    }

    /// Refresh the pivot on the active sheet on demand (issue #33 Part B).
    ///
    /// The menu/command the acceptance criteria ask for. Reports the group count
    /// in the status bar, or explains that the active sheet is not a pivot —
    /// never silently doing nothing.
    pub fn refresh_active_pivot(&mut self) {
        let active = self.wb.active_sheet();
        if !self.wb.is_pivot_sheet(active) {
            self.status = "The active sheet is not a pivot table".into();
            return;
        }
        match self.wb.refresh_pivot(active) {
            Some(groups) => {
                self.status = format!(
                    "Refreshed pivot · {} group{}",
                    fmt_int(groups),
                    if groups == 1 { "" } else { "s" }
                );
            }
            None => {
                // A pivot whose source sheet was deleted keeps its last result.
                self.status = "Pivot source is unavailable — showing the last result".into();
            }
        }
    }

    /// Whether sizing has unsaved changes.
    pub fn sizing_is_dirty(&self) -> bool {
        self.sizing_dirty
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
        // Issue #42: "format cells" is a granular allowance.
        if let Some(d) = self
            .wb
            .protection()
            .deny_action(ferrix_core::ProtectAction::FormatCells)
        {
            self.status = format!("Formatting refused — {d}");
            return;
        }
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

    // ============================ conditional formatting (roadmap #11) =====

    /// Apply cell decoration — borders, alignment, wrap, rotation (#28) — to
    /// the current selection.
    ///
    /// A SINGLE cell goes to the per-cell override, a RANGE goes to range
    /// scope, and a whole-column selection goes to COLUMN scope. That last
    /// case is the scale one: bordering column C must cost one entry, not one
    /// per row, and must keep applying to rows a later paste appends. The
    /// dispatch is here rather than in the store so the store never has to
    /// guess what the user meant by a selection.
    ///
    /// Layers over what is already there — `CellDecor::apply_to` semantics —
    /// so "add a bottom border" does not clear an alignment set a moment ago.
    pub fn apply_decor(&mut self, decor: ferrix_core::CellDecor) {
        // Formatting is a granular allowance, exactly as it is for typography.
        if let Some(d) = self
            .wb
            .protection()
            .deny_action(ferrix_core::ProtectAction::FormatCells)
        {
            self.status = format!("Formatting refused — {d}");
            return;
        }
        if decor.is_empty() {
            return;
        }
        let (a, b) = self.selection.bounds();
        let rows = self.wb.view().row_count() as u32;
        // "The whole column is selected" means the selection spans every row
        // that exists. Stored on the COLUMN so it also covers rows that do
        // not exist yet — which is the difference between a border that
        // survives an append and one that does not.
        let whole_cols = rows > 0 && a.row == 0 && b.row + 1 >= rows;
        if whole_cols {
            for col in a.col..=b.col {
                self.wb.format.set_column_decor(col, decor);
            }
        } else if a == b {
            self.wb.format.set_cell_decor(a, decor);
        } else {
            let range = ferrix_core::TableRange::new(a.row, a.col, b.row, b.col);
            self.wb.format.set_range_decor(range, decor);
        }
        self.wb.mark_dirty();
        self.status = "Cell formatting applied".into();
    }

    // ---- sparklines (issue #36) ----

    /// Add a sparkline group over the selection, drawing into the column
    /// immediately to its RIGHT.
    ///
    /// The selection is the SOURCE, and the destination is derived rather than
    /// asked for. That follows the precedent the border commands set: a
    /// command that needs a value waits for a dialog, and one that has an
    /// unambiguous answer just does it. "Beside the numbers" is where a
    /// sparkline column goes in every spreadsheet anyone has used.
    ///
    /// ONE entry is written however many rows the selection spans -- see
    /// `ferrix_core::sparkline`. `sparkline_over_a_100k_row_selection_stores_one_group`
    /// asserts it.
    pub fn add_sparkline(&mut self, kind: ferrix_core::SparkKind) {
        // A sparkline is formatting, so it answers to the same granular
        // allowance the other format commands do.
        if let Some(d) = self
            .wb
            .protection()
            .deny_action(ferrix_core::ProtectAction::FormatCells)
        {
            self.status = format!("Sparkline refused -- {d}");
            return;
        }
        let (a, b) = self.selection.bounds();
        if a.col == b.col {
            // One source column is one point per row, which draws a dot and
            // says nothing. Refusing with a sentence beats painting a column
            // of specks the user cannot interpret.
            self.status =
                "Select at least two columns of numbers -- a sparkline plots a row of them".into();
            return;
        }
        let target_col = b.col + 1;
        let group = ferrix_core::SparkGroup::new(
            kind,
            ferrix_core::TableRange::new(a.row, target_col, b.row, target_col),
            a.col,
            b.col,
        );
        self.wb.sparklines.add(group);
        self.wb.mark_dirty();
        let rows = (b.row - a.row + 1) as u64;
        self.status = format!(
            "{} sparklines in column {} over {rows} row{}",
            kind.label(),
            ferrix_core::column_name(target_col),
            if rows == 1 { "" } else { "s" }
        );
    }

    /// Remove every sparkline group drawing inside the selection.
    pub fn clear_sparklines(&mut self) {
        let (a, b) = self.selection.bounds();
        let range = ferrix_core::TableRange::new(a.row, a.col, b.row, b.col);
        let n = self.wb.sparklines.clear_in(range);
        if n == 0 {
            self.status = "No sparklines in the selection".into();
            return;
        }
        self.wb.mark_dirty();
        self.status = format!(
            "Removed {n} sparkline group{}",
            if n == 1 { "" } else { "s" }
        );
    }

    /// How many sparkline GROUPS are configured. A function of how many the
    /// user made, never of how many rows they cover.
    pub fn sparkline_group_count(&self) -> usize {
        self.wb.sparklines.len()
    }

    /// Widen the single configured sparkline group down to `last_row`.
    ///
    /// For the scale test only. There is no gesture that selects 200M rows,
    /// and materialising them to build one is precisely what the invariant
    /// forbids -- so the group is widened here and the assertion is still made
    /// on what the PAINT LOOP does with it.
    #[cfg(test)]
    pub fn widen_sparkline_for_test(&mut self, last_row: u32) {
        let Some(g) = self.wb.sparklines.iter().next().copied() else {
            panic!("a group must already be configured");
        };
        self.wb.sparklines.clear_in(g.target);
        self.wb.sparklines.add(ferrix_core::SparkGroup::new(
            g.kind,
            ferrix_core::TableRange::new(
                g.target.first_row,
                g.target.first_col,
                last_row,
                g.target.last_col,
            ),
            g.src_first_col,
            g.src_last_col,
        ));
    }

    /// The decoration a cell resolves to right now, through the same
    /// `SheetFormat` the grid paints from.
    pub fn decor_at(&self, cell: CellRef) -> ferrix_core::CellDecor {
        self.wb.format.decor_at(cell)
    }

    /// Scopes carrying decoration. The number the scale criterion asserts on:
    /// a function of how many formats the user applied, never of how many
    /// rows they cover.
    pub fn decor_count(&self) -> usize {
        self.wb.format.decor_count()
    }

    // ============================ data validation & autocomplete (#41) =====

    /// The rectangle the current selection means, for a validation rule.
    fn selection_range(&self) -> ferrix_core::TableRange {
        let (a, b) = self.selection.bounds();
        ferrix_core::TableRange::new(a.row, a.col, b.row, b.col)
    }

    /// Open the New Rule dialog on the current selection.
    pub fn validation_new_rule(&mut self) {
        self.validation = Some(crate::validation_panel::ValidationState::new_rule(
            self.selection_range(),
        ));
    }

    /// Open the Manage Rules list.
    pub fn validation_manage(&mut self) {
        self.validation = Some(crate::validation_panel::ValidationState::manage(
            self.selection_range(),
        ));
    }

    /// Remove every rule covering the selection.
    pub fn validation_clear_selection(&mut self) {
        let range = self.selection_range();
        let n = self.wb.validation.clear_overlapping(range);
        self.wb.mark_dirty();
        // The circles were computed against rules that no longer exist.
        self.circled.clear();
        self.status = match n {
            0 => format!("No validation rules cover {}", range.to_a1()),
            1 => format!("Validation removed from {}", range.to_a1()),
            _ => format!("{n} validation rules removed from {}", range.to_a1()),
        };
    }

    /// Turn the Circle Invalid Data overlay on, recomputing it this frame.
    pub fn circle_invalid_data(&mut self) {
        self.circle_invalid = true;
        self.refresh_circles();
        self.status = if self.wb.validation.is_empty() {
            "No validation rules to check - add one first".to_string()
        } else if self.circled.is_empty() {
            "No invalid data in view".to_string()
        } else {
            format!(
                "{} invalid cell{} circled in the current view",
                self.circled.len(),
                if self.circled.len() == 1 { "" } else { "s" }
            )
        };
    }

    pub fn clear_validation_circles(&mut self) {
        self.circle_invalid = false;
        self.circled.clear();
        self.status = "Validation circles cleared".into();
    }

    pub fn toggle_autocomplete(&mut self) {
        self.autocomplete_on = !self.autocomplete_on;
        if !self.autocomplete_on {
            self.autocomplete.dismiss();
        }
        self.status = format!(
            "Value suggestions {}",
            if self.autocomplete_on { "on" } else { "off" }
        );
    }

    /// Recompute the circled set from the VIEWPORT.
    ///
    /// The bound the acceptance criterion names, and the reason this is not
    /// simply `invalid_cells_in(whole_sheet)`: the range handed to the workbook
    /// is the rows the last frame actually painted, so the pass is a screenful
    /// of work however many rows the sheet has. `last_painted_rows` is real
    /// paint output - the same list the paint loop walked - not a
    /// recomputation, so this cannot drift from what is on screen.
    fn refresh_circles(&mut self) {
        self.circled.clear();
        if !self.circle_invalid || self.wb.validation.is_empty() {
            return;
        }
        let Some((first_row, last_row)) = self.visible_row_span() else {
            return;
        };
        let cols = self.wb.view().col_count().max(1) as u32;
        let range = ferrix_core::TableRange::new(first_row, 0, last_row, cols.saturating_sub(1));
        // A hard cap on top of the viewport bound: even a caller that widened
        // the range cannot make this allocate without limit.
        self.circled = self.wb.invalid_cells_in(range, MAX_CIRCLED);
    }

    /// The row span the grid painted last frame, if it has painted one.
    fn visible_row_span(&self) -> Option<(u32, u32)> {
        let first = self.last_painted_rows.iter().map(|(_, r)| *r).min()?;
        let last = self.last_painted_rows.iter().map(|(_, r)| *r).max()?;
        Some((first, last))
    }

    /// Cells circled right now. Read by tests and by the paint loop.
    pub fn circled_cells(&self) -> &[CellRef] {
        &self.circled
    }

    /// Validation circles the last frame actually PAINTED.
    ///
    /// The specific shape this feature adds, counted where it is drawn -
    /// deliberately not a total shape count, which selection, borders and
    /// comment markers all move for unrelated reasons.
    pub fn painted_validation_circles(&self) -> usize {
        self.last_validation_circles
    }

    pub fn validation_is_open(&self) -> bool {
        self.validation.is_some()
    }

    pub fn validation_state(&self) -> Option<&crate::validation_panel::ValidationState> {
        self.validation.as_ref()
    }

    pub fn validation_state_mut(
        &mut self,
    ) -> Option<&mut crate::validation_panel::ValidationState> {
        self.validation.as_mut()
    }

    /// The suggestion popup's state, for tests and the paint loop.
    pub fn autocomplete_state(&self) -> &crate::validation_panel::AutocompleteState {
        &self.autocomplete
    }

    /// Apply an outcome from the validation dialog.
    fn validation_apply(&mut self, out: crate::validation_panel::ValidationOutcome) {
        use crate::validation_panel::{ValidationForm, ValidationMode, ValidationState};
        let Some(st) = self.validation.as_mut() else {
            return;
        };
        let range = st.range;
        if out.cancel {
            self.validation = None;
            return;
        }
        if out.new_rule {
            *st = ValidationState::new_rule(range);
        }
        if let Some(i) = out.edit {
            if let Some(rule) = self.wb.validation.get(i) {
                let form = ValidationForm::from_rule(rule);
                let r = rule.range;
                if let Some(st) = self.validation.as_mut() {
                    st.form = form;
                    st.range = r;
                    st.mode = ValidationMode::Edit(i);
                }
            }
        }
        if out.back {
            if let Some(st) = self.validation.as_mut() {
                st.mode = ValidationMode::Manage;
            }
        }
        if let Some(i) = out.delete {
            if self.wb.validation.remove(i).is_some() {
                self.wb.mark_dirty();
                self.circled.clear();
                self.status = "Validation rule deleted".into();
            }
        }
        if out.commit {
            let st = self.validation.as_ref().expect("checked above");
            let rule = st.form.to_validation(st.range);
            let mode = st.mode;
            let loss = ferrix_io::sheet_validation_xlsx_loss(&rule);
            let label = rule.range.to_a1();
            let domain = rule.domain.label();
            let stored = match mode {
                ValidationMode::Edit(i) => self.wb.validation.set(i, rule),
                _ => self.wb.validation.push(rule).is_some(),
            };
            self.status = if !stored {
                "Too many validation rules on this sheet".to_string()
            } else {
                self.wb.mark_dirty();
                // The circles were computed against the previous rule set.
                self.refresh_circles();
                match loss.first() {
                    Some(w) => format!("{domain} validation on {label} - note: {w}"),
                    None => format!("{domain} validation applied to {label}"),
                }
            };
            self.validation = Some(ValidationState::manage(range));
        }
    }

    /// Draw the validation editor and act on what it reported.
    fn show_validation_editor(&mut self, ctx: &egui::Context) {
        let th = self.theme;
        // Drawn against a detached clone, so the read path is provably
        // read-only and every mutation arrives as an outcome - the same
        // discipline `show_cond_editor` uses.
        let Some(mut st) = self.validation.clone() else {
            return;
        };
        let out = crate::validation_panel::show(ctx, &mut st, &self.wb.validation, th);
        if self.validation.is_some() {
            self.validation = Some(st);
        }
        if !out.is_empty() {
            self.validation_apply(out);
        }
    }

    /// Refresh the suggestion popup for the live edit.
    ///
    /// Called once per frame while a cell is open for editing. The scan it
    /// triggers is bounded by `SCAN_LIMIT`, so this is a fixed cost per frame
    /// and not a function of the column's length.
    fn refresh_autocomplete(&mut self) {
        let Some(cell) = self.editing else {
            self.autocomplete.reset();
            return;
        };
        if !self.autocomplete_on || self.autocomplete.dismissed {
            return;
        }
        // A formula is not a column value; suggesting one mid-formula would
        // fight the reference highlighting.
        if self.edit_buffer.trim_start().starts_with('=') {
            self.autocomplete.cell = None;
            return;
        }
        let (s, from_list, _) = self.wb.suggest(cell, &self.edit_buffer);
        self.autocomplete.offer(cell, s, from_list);
    }

    /// Accept the highlighted suggestion into the edit buffer.
    ///
    /// Returns whether anything was accepted, so the caller can tell an
    /// accepted Tab from one that should move the selection.
    fn accept_suggestion(&mut self) -> bool {
        let Some(text) = self.autocomplete.current().map(str::to_string) else {
            return false;
        };
        self.edit_buffer.clone_from(&text);
        self.formula_input.clone_from(&text);
        self.edit_caret = self.edit_buffer.len();
        self.pending_caret = Some(self.edit_caret);
        self.autocomplete.dismiss();
        // Dismissing sets `dismissed`, which would suppress the popup for the
        // rest of THIS edit. Accepting is not a rejection, so clear it - the
        // user may keep typing and want more help.
        self.autocomplete.dismissed = false;
        self.recompute_formula();
        true
    }

    /// Open the in-cell dropdown on `cell`, listing its rule's allowed values.
    ///
    /// Begins an edit seeded with an EMPTY buffer so every allowed value is
    /// offered; the cell's current value is untouched until the user picks
    /// one, and Escape puts it back through the ordinary `edit_pre_text`
    /// snapshot.
    pub fn open_validation_dropdown(&mut self, cell: CellRef) {
        let Some(values) = self.wb.dropdown_for(cell).map(<[String]>::to_vec) else {
            return;
        };
        self.begin_edit(cell, Some(String::new()));
        self.autocomplete.reset();
        self.autocomplete
            .offer(cell, Suggestions::from_list(&values, ""), true);
    }

    /// Escape while the popup is open: close it and change NOTHING else.
    ///
    /// Returns whether the popup absorbed the Escape, so the caller knows not
    /// to also cancel the edit. This is the acceptance criterion "Escape
    /// dismisses without altering the typed text", and it holds structurally:
    /// `AutocompleteState::dismiss` has no access to the edit buffer.
    fn dismiss_autocomplete(&mut self) -> bool {
        if !self.autocomplete.is_open() {
            return false;
        }
        self.autocomplete.dismiss();
        true
    }

    /// Open the New Rule dialog on the current selection.
    pub fn cond_new_rule(&mut self) {
        let (a, b) = self.selection.bounds();
        self.cond = Some(crate::cond_format::CondFormatState::new_rule(
            crate::cond_format::CondTarget::from_selection(a, b),
        ));
    }

    /// Open the Manage Rules list for the current selection.
    ///
    /// Prefers whichever scope ALREADY HAS RULES. A user who put a rule on the
    /// whole column and then clicked one cell in it must not be told there are
    /// no rules — that reads as data loss, and it is the single most likely
    /// way for this dialog to lie.
    pub fn cond_manage(&mut self) {
        let (a, b) = self.selection.bounds();
        let range = crate::cond_format::CondTarget::from_selection(a, b);
        let target = if range.rules(&self.wb.format).is_empty() {
            let col = range.widen();
            if col.rules(&self.wb.format).is_empty() {
                range
            } else {
                col
            }
        } else {
            range
        };
        self.cond = Some(crate::cond_format::CondFormatState::manage(target));
    }

    /// Whether the editor is open. Read by tests and by the frame loop.
    pub fn cond_is_open(&self) -> bool {
        self.cond.is_some()
    }

    pub fn cond_state(&self) -> Option<&crate::cond_format::CondFormatState> {
        self.cond.as_ref()
    }

    pub fn cond_state_mut(&mut self) -> Option<&mut crate::cond_format::CondFormatState> {
        self.cond.as_mut()
    }

    /// The lossy-export warning currently in force, if any.
    pub fn cond_warning(&self) -> Option<&str> {
        self.cond_warning.as_deref()
    }

    // ================================== goal seek (issue #35) ==============

    /// Open the Goal Seek dialog, seeded from the current selection.
    ///
    /// "Set cell" defaults to the cursor because the user has almost always
    /// just clicked the number they want to change. "By changing cell" is left
    /// blank on purpose: guessing it would be guessing which input drives the
    /// model, and a wrong guess silently pointed at the wrong cell is worse
    /// than an empty field.
    pub fn goal_seek_open(&mut self) {
        let cursor = self.selection.cursor;
        self.goal_seek = Some(GoalSeekState {
            set_cell: cell_label(cursor),
            to_value: String::new(),
            by_changing: String::new(),
            ..Default::default()
        });
    }

    pub fn goal_seek_is_open(&self) -> bool {
        self.goal_seek.is_some()
    }

    pub fn goal_seek_state(&self) -> Option<&GoalSeekState> {
        self.goal_seek.as_ref()
    }

    pub fn goal_seek_state_mut(&mut self) -> Option<&mut GoalSeekState> {
        self.goal_seek.as_mut()
    }

    /// The dialog's result line, if a run has produced one.
    pub fn goal_seek_message(&self) -> Option<&str> {
        self.goal_seek
            .as_ref()
            .and_then(|s| s.message.as_ref())
            .map(|(t, _)| t.as_str())
    }

    /// Run the solver on whatever is in the dialog's fields.
    ///
    /// Every failure — an unparseable reference, a non-numeric target, a
    /// refusal from the solver — lands in the dialog's own message rather than
    /// only in the status bar, because the dialog is what the user is looking
    /// at and it stays open so the input can be corrected.
    pub fn goal_seek_solve(&mut self) {
        let Some(st) = self.goal_seek.as_ref() else {
            return;
        };
        let (set_cell, to_value, by_changing) = (
            st.set_cell.trim().to_string(),
            st.to_value.trim().to_string(),
            st.by_changing.trim().to_string(),
        );

        let fail = |app: &mut Self, msg: String| {
            if let Some(s) = app.goal_seek.as_mut() {
                s.message = Some((msg.clone(), true));
            }
            app.status = msg;
        };

        let Some(target) = CellRef::from_a1(&set_cell) else {
            fail(
                self,
                format!("Set cell: {set_cell:?} is not a cell like B4"),
            );
            return;
        };
        let Ok(value) = to_value.parse::<f64>() else {
            fail(self, format!("To value: {to_value:?} is not a number"));
            return;
        };
        let Some(changing) = CellRef::from_a1(&by_changing) else {
            fail(
                self,
                format!("By changing cell: {by_changing:?} is not a cell like B4"),
            );
            return;
        };
        if target == changing {
            fail(
                self,
                "Set cell and By changing cell must be different".to_string(),
            );
            return;
        }

        match self.wb.goal_seek(target, value, changing) {
            Err(crate::workbook::GoalSeekError::NotDependent) => {
                // The real explanation the issue asks for, not "did not
                // converge": nothing was ever going to converge, and saying so
                // points the user at the actual mistake.
                fail(
                    self,
                    format!(
                        "{} does not depend on {} — changing {} cannot move it. \
                         Check the formula, or pick a cell {} actually reads.",
                        cell_label(target),
                        cell_label(changing),
                        cell_label(changing),
                        cell_label(target)
                    ),
                );
            }
            Err(crate::workbook::GoalSeekError::ChangingCellIsFormula) => {
                fail(
                    self,
                    format!(
                        "{} holds a formula. Goal Seek would have to overwrite it \
                         with a number; pick an input cell instead.",
                        cell_label(changing)
                    ),
                );
            }
            Ok(report) => {
                let msg = if report.converged {
                    format!(
                        "{} = {} with {} = {} ({} iteration{})",
                        cell_label(target),
                        ferrix_core::format_number(report.final_a.unwrap_or(report.target)),
                        cell_label(changing),
                        ferrix_core::format_number(report.final_b),
                        report.iterations,
                        if report.iterations == 1 { "" } else { "s" }
                    )
                } else {
                    // Non-convergence reports what was ACTUALLY reached, never
                    // the requested value: claiming success here is the exact
                    // failure the issue calls out.
                    match report.final_a {
                        Some(a) => format!(
                            "No solution found after {} iterations. Closest: {} = {} \
                             with {} = {} (wanted {}).",
                            report.iterations,
                            cell_label(target),
                            ferrix_core::format_number(a),
                            cell_label(changing),
                            ferrix_core::format_number(report.final_b),
                            ferrix_core::format_number(report.target)
                        ),
                        None => format!(
                            "{} never evaluated to a number, so there was nothing to \
                             solve for. {} is unchanged.",
                            cell_label(target),
                            cell_label(changing)
                        ),
                    }
                };
                self.status = msg.clone();
                if let Some(s) = self.goal_seek.as_mut() {
                    s.message = Some((msg, !report.converged));
                    // Only a run that produced a number committed an edit;
                    // `final_a == None` restores and commits nothing, so
                    // Cancel must not try to undo it.
                    s.applied = report.final_a.is_some();
                }
                // Show the user the cell that moved.
                self.set_selection(Selection::single(changing));
                self.sync_formula_bar();
            }
        }
    }

    /// Cancel the dialog, restoring the changing cell if a run applied one.
    ///
    /// Because the whole run is a single undo entry (see
    /// `Workbook::goal_seek`), one `undo()` puts the changing cell AND every
    /// dependent back — Cancel does not need to remember the old value itself.
    pub fn goal_seek_cancel(&mut self) {
        let applied = self.goal_seek.as_ref().is_some_and(|s| s.applied);
        if applied {
            if let Some(cell) = self.wb.undo() {
                self.set_selection(Selection::single(cell));
                self.status = "Goal Seek cancelled — the changing cell was restored".into();
            }
            self.sync_formula_bar();
        }
        self.goal_seek = None;
    }

    /// Close the dialog, KEEPING whatever the run applied.
    pub fn goal_seek_close(&mut self) {
        self.goal_seek = None;
    }

    // ---- protection (issue #42) ----
    //
    // Every one of these is the SAME entry point the menu and the palette
    // reach through `run_command`, so a test that drives them drives the
    // production path rather than a parallel one.

    /// Mark the selection locked (the default state).
    pub fn lock_selection(&mut self) {
        let (tl, br) = self.selection.bounds();
        let range = ferrix_core::TableRange::new(tl.row, tl.col, br.row, br.col);
        self.wb.protection_mut().lock_range(range);
        let enforced = self.wb.protection().is_enabled();
        self.status = if enforced {
            format!(
                "{} locked — this sheet is protected, so edits there are refused",
                range.to_a1()
            )
        } else {
            format!(
                "{} locked — but this sheet is not protected yet, so the lock does nothing. \
                 Data ▸ Protect Sheet to make it bite.",
                range.to_a1()
            )
        };
    }

    /// Mark the selection unlocked (editable while protected).
    pub fn unlock_selection(&mut self) {
        let (tl, br) = self.selection.bounds();
        let range = ferrix_core::TableRange::new(tl.row, tl.col, br.row, br.col);
        self.wb.protection_mut().unlock_range(range);
        self.status = format!(
            "{} unlocked — editable even while the sheet is protected",
            range.to_a1()
        );
    }

    /// Open the Protect Sheet dialog — or, if the sheet is already protected,
    /// the Unprotect one.
    pub fn protect_sheet_open(&mut self) {
        use crate::protect_panel::{ProtectDialog, ProtectTarget};
        self.protect_dialog = Some(if self.wb.protection().is_enabled() {
            ProtectDialog::unprotect(ProtectTarget::Sheet)
        } else {
            ProtectDialog::for_sheet(*self.wb.protection().allow())
        });
    }

    pub fn protect_workbook_open(&mut self) {
        use crate::protect_panel::{ProtectDialog, ProtectTarget};
        self.protect_dialog = Some(if self.wb.workbook_protection().structure_locked() {
            ProtectDialog::unprotect(ProtectTarget::Workbook)
        } else {
            ProtectDialog::for_workbook()
        });
    }

    pub fn protect_dialog_is_open(&self) -> bool {
        self.protect_dialog.is_some()
    }

    pub fn protect_dialog_state(&self) -> Option<&crate::protect_panel::ProtectDialog> {
        self.protect_dialog.as_ref()
    }

    pub fn protect_dialog_state_mut(&mut self) -> Option<&mut crate::protect_panel::ProtectDialog> {
        self.protect_dialog.as_mut()
    }

    pub fn protect_dialog_close(&mut self) {
        self.protect_dialog = None;
    }

    /// Apply the dialog — the same call its Apply button makes.
    ///
    /// Returns false and leaves the dialog open when an unprotect was refused,
    /// so a wrong password does not silently close the window.
    pub fn protect_dialog_apply(&mut self) -> bool {
        use crate::protect_panel::ProtectTarget;
        let Some(d) = self.protect_dialog.take() else {
            return false;
        };
        match (d.target, d.unprotecting) {
            (ProtectTarget::Sheet, false) => {
                let hash = d.hash();
                self.wb.protection_mut().protect(d.allow, hash);
                self.status = format!(
                    "Sheet protected — {}",
                    crate::protect_panel::DETERRENT_SHORT
                );
                true
            }
            (ProtectTarget::Sheet, true) => {
                let expected = self.wb.protection().hash();
                if !d.unlock_matches(expected) {
                    let mut d = d;
                    d.message = Some(
                        "That password does not match the one stored in this file. \
                         (A 16-bit hash cannot really check a password — Ferrix asks \
                         because Excel does.)"
                            .to_string(),
                    );
                    self.protect_dialog = Some(d);
                    return false;
                }
                self.wb.protection_mut().unprotect();
                self.status = "Sheet unprotected — every cell is editable again".to_string();
                true
            }
            (ProtectTarget::Workbook, false) => {
                let hash = d.hash();
                self.wb.workbook_protection_mut().protect_structure(hash);
                self.status = format!(
                    "Workbook structure protected — sheets cannot be added, deleted, renamed \
                     or reordered. {}",
                    crate::protect_panel::DETERRENT_SHORT
                );
                true
            }
            (ProtectTarget::Workbook, true) => {
                let expected = self.wb.workbook_protection().hash();
                if !d.unlock_matches(expected) {
                    let mut d = d;
                    d.message = Some("That password does not match.".to_string());
                    self.protect_dialog = Some(d);
                    return false;
                }
                self.wb.workbook_protection_mut().unprotect();
                self.status = "Workbook structure unprotected".to_string();
                true
            }
        }
    }

    /// What the status bar says about the cell under the cursor.
    ///
    /// Surfacing the "locked but not yet protected" state is an explicit
    /// acceptance criterion — it is the thing that trips people up.
    pub fn cursor_lock_note(&self) -> &'static str {
        let cell = self.wb.merges.resolve(self.selection.cursor);
        self.wb.protection().state_of(cell).explain()
    }

    /// Draw the Protect dialog. Kept beside the other windows in this file.
    fn show_protect_dialog(&mut self, ctx: &egui::Context) {
        use crate::protect_panel::DETERRENT_NOTICE;
        let Some(mut d) = self.protect_dialog.take() else {
            return;
        };
        let th = self.theme;
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;

        egui::Window::new(d.title())
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                // THE NOTICE. First thing in the window, in full, always.
                ui.label(RichText::new(DETERRENT_NOTICE).color(th.error).size(12.0));
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);

                if d.unprotecting {
                    ui.label("Password (if the file has one):");
                    ui.add(
                        egui::TextEdit::singleline(&mut d.password)
                            .password(true)
                            .desired_width(220.0),
                    );
                } else {
                    ui.label(RichText::new(
                        "Cells are LOCKED by default. Only the ranges you explicitly \
                         unlock (Data ▸ Unlock cells) stay editable once this is on.",
                    ));
                    ui.add_space(6.0);
                    ui.label("Password to unprotect (optional):");
                    ui.add(
                        egui::TextEdit::singleline(&mut d.password)
                            .password(true)
                            .desired_width(220.0),
                    );
                    if d.target == crate::protect_panel::ProtectTarget::Sheet {
                        ui.add_space(8.0);
                        ui.label(RichText::new("Allow users of this sheet to:").strong());
                        for (label, flag) in d.allowance_rows() {
                            ui.checkbox(flag, label);
                        }
                    } else {
                        ui.add_space(8.0);
                        ui.label(
                            "Sheets cannot be added, deleted, renamed or reordered while this \
                             is on.",
                        );
                    }
                }

                if let Some(msg) = &d.message {
                    ui.add_space(6.0);
                    ui.label(RichText::new(msg).color(th.error));
                }

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let label = if d.unprotecting {
                        "Unprotect"
                    } else {
                        "Protect"
                    };
                    if ui.button(label).clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        self.protect_dialog = Some(d);
        if cancel || !open {
            self.protect_dialog = None;
        } else if apply {
            self.protect_dialog_apply();
        }
    }

    /// The format the grid should paint with THIS FRAME.
    ///
    /// Either the real store, or — while a preview is live — a clone with the
    /// pending rule spliced in. The clone is a handful of rules; it is never a
    /// function of the row count, and it exists only while a modal is open.
    /// The real store is not touched, which is what makes Cancel a no-op
    /// rather than an undo.
    fn cond_preview_format(&self) -> Option<ferrix_core::SheetFormat> {
        self.cond
            .as_ref()
            .and_then(|c| c.preview_format(&self.wb.format))
    }

    /// Apply an outcome from the dialog. Returns whether anything was stored.
    fn cond_apply(&mut self, out: crate::cond_format::CondOutcome) -> bool {
        use crate::cond_format::{CondFormatState, CondMode, RuleForm};
        let Some(st) = self.cond.as_mut() else {
            return false;
        };
        let target = st.target;
        let mut stored = false;

        if out.cancel {
            // Nothing to roll back: the preview never wrote anything.
            self.cond = None;
            return false;
        }
        if let Some(t) = out.retarget {
            st.target = t;
        }
        if out.new_rule {
            let t = st.target;
            *st = CondFormatState::new_rule(t);
        }
        if let Some(i) = out.edit {
            if let Some(rule) = target.rules(&self.wb.format).get(i) {
                st.form = RuleForm::from_rule(rule);
                st.mode = CondMode::Edit(i);
                st.preview = true;
            }
        }
        if out.back {
            let t = st.target;
            *st = CondFormatState::manage(t);
        }
        if let Some((i, delta)) = out.move_rule {
            if target.move_rule(&mut self.wb.format, i, delta) {
                self.wb.mark_dirty();
                stored = true;
                self.status = format!("Rule moved {} in precedence", pos_word(delta));
            }
        }
        if let Some(i) = out.delete {
            if target.remove(&mut self.wb.format, i) {
                self.wb.mark_dirty();
                stored = true;
                self.status = format!("Rule deleted from {}", target.label());
            }
        }
        if out.commit {
            let st = self.cond.as_ref().expect("checked above");
            let rule = st.form.to_rule();
            let mode = st.mode;
            let warn = crate::cond_format::xlsx_warning(&rule);
            match mode {
                CondMode::Edit(i) if target.replace(&mut self.wb.format, i, rule.clone()) => {}
                // An index that no longer exists appends rather than silently
                // dropping the user's work.
                _ => target.push(&mut self.wb.format, rule.clone()),
            }
            self.wb.mark_dirty();
            stored = true;
            self.status = match &warn {
                Some(w) => format!("{} on {} · {w}", rule.label(), target.label()),
                None => format!("{} applied to {}", rule.label(), target.label()),
            };
            self.cond_warning = warn;
            // Straight back to the list, so a second rule is one click away
            // and the new precedence order is immediately visible.
            self.cond = Some(CondFormatState::manage(target));
        }
        stored
    }

    /// Draw the editor and act on what it reported.
    fn show_cond_editor(&mut self, ctx: &egui::Context) {
        let th = self.theme;
        // The dialog borrows the format to LIST rules, so it is drawn against
        // a detached state and every mutation comes back as an outcome. That
        // is also what makes the read path provably read-only.
        let Some(mut st) = self.cond.clone() else {
            return;
        };
        let out = crate::cond_format::show(ctx, &mut st, &self.wb.format, th);
        // Field edits (colours, the kind selector, the preview checkbox) live
        // on the state itself and are adopted whatever the outcome says.
        if self.cond.is_some() {
            self.cond = Some(st);
        }
        if !out.is_empty() {
            self.cond_apply(out);
        }
    }

    /// The style a cell resolves to RIGHT NOW, through exactly the format the
    /// grid is painting from this frame — the preview clone while a dialog is
    /// previewing, the real store otherwise.
    ///
    /// This is the app's own answer to "what does this cell look like", and it
    /// is what the editor's tests assert on. Asserting that a rule appears in a
    /// list would pass against an editor that stores rules nothing ever reads;
    /// asserting the resolved style cannot.
    ///
    /// Window-dependent rules (colour scales, data bars, top/bottom-N) are
    /// evaluated over `window` — pass the rows the caller means, since "the
    /// visible window" is a property of the frame, not of a cell.
    pub fn resolved_style(
        &self,
        cell: CellRef,
        window: std::ops::Range<u32>,
    ) -> ferrix_core::CellStyle {
        let fmt_owned = self.cond_preview_format();
        let fmt = fmt_owned.as_ref().unwrap_or(&self.wb.format);
        let view = self.wb.view();
        let mut plan = Vec::new();
        fmt.plan(cell.col, &mut plan);
        let mut evals = Vec::new();
        if ferrix_core::SheetFormat::plan_needs_window(&plan) {
            let mut vals: Vec<f64> = Vec::new();
            for r in window {
                if let ferrix_core::Value::Number(n) = view.get(CellRef::new(r, cell.col)) {
                    vals.push(n);
                }
            }
            for e in &plan {
                let mut scratch = vals.clone();
                evals.push(ferrix_core::RuleEval::for_rule(e.rule, &mut scratch));
            }
        }
        let value = view.get(cell);
        let text = if ferrix_core::SheetFormat::plan_needs_text(&plan) {
            view.display(cell)
        } else {
            String::new()
        };
        fmt.resolve(cell, &value, &text, &plan, &evals)
    }

    /// How many conditional rules are configured on this sheet, across every
    /// scope. The number the scale invariant is asserted on: it is a function
    /// of how many rules the user made, never of how many rows they cover.
    pub fn rule_count(&self) -> usize {
        self.wb.format.rule_count()
    }

    /// A snapshot of all sheet formatting, for tests that need to prove Cancel
    /// changed nothing. `SheetFormat` is `PartialEq`, so this compares whole.
    pub fn format_snapshot(&self) -> ferrix_core::SheetFormat {
        self.wb.format.clone()
    }

    /// Commit a value into a cell through the app's real edit path.
    ///
    /// Used by tests that need a specific value or formula in place before
    /// exercising something else. Goes through `Workbook::commit_edit`, so the
    /// dependency graph and recalculation happen exactly as they do for a
    /// typed edit.
    pub fn commit_edit_for_test(&mut self, cell: CellRef, text: &str) {
        self.wb.commit_edit(cell, text);
        self.wb.end_edit_run();
    }

    /// The formula SOURCE behind a cell, or `None` if it holds a literal.
    ///
    /// The distinction Paste Values depends on: after pasting values, a cell
    /// showing `15` must have no formula behind it.
    pub fn formula_src_at_for_test(&self, cell: CellRef) -> Option<String> {
        self.wb.formula_src_at(cell)
    }

    /// Apply a number format to the selection, as the Format menu does.
    pub fn apply_number_format_for_test(&mut self, fmt: ferrix_core::NumberFormat) {
        let (a, b) = self.selection.bounds();
        if a == b {
            let mut ov = self.wb.format.cell_override(a).cloned().unwrap_or_default();
            ov.format = Some(fmt);
            self.wb.format.set_cell_override(a, ov);
        } else {
            let range = ferrix_core::TableRange::new(a.row, a.col, b.row, b.col);
            let mut rf = ferrix_core::RangeFormat::new(range);
            rf.format = Some(fmt);
            self.wb.format.push_range(rf);
        }
        self.wb.mark_dirty();
    }

    /// Bytes the sheet's format store holds, for the scale assertions.
    pub fn format_heap_bytes(&self) -> usize {
        self.wb.format.heap_bytes()
    }

    /// Direct access to the format store, for tests that drive a Manage-list
    /// action (reorder, delete) whose button lives at a pixel that moves with
    /// the theme. The same `SheetFormat` the dialog's buttons mutate.
    pub fn format_mut_for_test(&mut self) -> &mut ferrix_core::SheetFormat {
        self.wb.mark_dirty();
        &mut self.wb.format
    }

    /// Close the editor without touching anything, for tests that need the
    /// dialog's own chrome out of the frame before counting shapes.
    pub fn cond_close_for_test(&mut self) {
        self.cond = None;
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

    /// Write this sheet as a Parquet file.
    ///
    /// Runs on the calling thread rather than the background export machinery
    /// the CSV path uses. That is a deliberate limitation, not an oversight:
    /// the progress/cancel plumbing is built around `ExportStats`, and wiring
    /// a second stats type through it is a bigger change than this issue.
    /// The write itself is still streaming — `export_parquet` holds one
    /// column stripe — so the memory bound holds; only the UI's
    /// responsiveness during a very large export does not.
    fn export_parquet_dialog(&mut self) {
        if self.exporting {
            self.status = "An export is already running — cancel it first".into();
            return;
        }
        let cost = crate::sheet_view::OwnedSheet::snapshot_cost_bytes(&self.wb.overlay);
        if let Err(msg) = ferrix_core::Budget::sample().admit(cost, "Exporting this sheet's edits")
        {
            self.status = msg;
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Parquet", &["parquet"])
            .set_file_name("export.parquet")
            .save_file()
        else {
            return;
        };

        let snapshot = crate::sheet_view::OwnedSheet::new(
            std::sync::Arc::clone(&self.wb.base),
            &self.wb.overlay,
        );
        // Columns the user has formatted as dates are written as timestamps;
        // everything else keeps its inferred type. Guessing from the magnitude
        // of the number instead would silently turn a price column into 2023.
        let date_columns: Vec<usize> = (0..snapshot.view().col_count())
            .filter(|c| self.column_is_date_formatted(*c))
            .collect();
        let opts = ferrix_io::ExportOptions {
            date_columns,
            use_headers: true,
        };

        self.status = match ferrix_io::export_parquet(&snapshot, &path, &opts) {
            Ok((stats, report)) => {
                // Report the lossy case rather than letting the user discover
                // it in pandas — the `rule_survives_xlsx` convention.
                let note = if report.is_lossless() {
                    String::new()
                } else {
                    format!(
                        " · {} mixed-type column(s) written as text: {}",
                        report.mixed_columns.len(),
                        report
                            .mixed_columns
                            .iter()
                            .map(|c| ferrix_core::column_name(*c as u32))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                format!(
                    "Exported {} rows × {} cols to Parquet ({} row group(s), {:.1} MB){}",
                    fmt_int(stats.rows),
                    stats.cols,
                    stats.row_groups,
                    stats.bytes as f64 / 1e6,
                    note
                )
            }
            Err(e) => format!("Parquet export failed: {e}"),
        };
    }

    /// Does this column's number format render as a date?
    ///
    /// The only signal available: `Value` has no date type, so a date is an
    /// f64 serial and the column's *number format* is what says it is a date.
    /// Stored per column, so asking this costs one map lookup regardless of
    /// how many rows the column has.
    fn column_is_date_formatted(&self, col: usize) -> bool {
        self.wb
            .format
            .column(col as u32)
            .is_some_and(|f| matches!(f.format, ferrix_core::NumberFormat::Date(_)))
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
        self.export_xlsx_to(&path);
    }

    /// The body of the xlsx export, with the file picker factored out.
    ///
    /// Split so a test can drive the REAL export the menu item runs. Testing
    /// `ferrix_io::export_workbook_full` directly would pass even if this
    /// method called a variant that silently dropped protection — which is
    /// exactly the bug this split exists to catch (issue #42).
    pub fn export_xlsx_to(&mut self, path: &std::path::Path) {
        // Only the in-RAM path can be handed to the writer; a mapped base has
        // no `Sheet` to give it.
        let BaseData::Memory(sheet) = &*self.wb.base else {
            self.status =
                "xlsx export of memory-mapped data is not supported yet — export CSV instead"
                    .into();
            return;
        };
        // Protection must survive the round trip (issue #42): a workbook
        // opened with a protected sheet and re-exported must still be
        // protected, including a password hash we only ever saw as a hash.
        // `export_workbook_full` is the variant that writes
        // `<sheetProtection>` and injects `<workbookProtection>`; the
        // plain `_with_names` call this used to make silently stripped both.
        self.status = match ferrix_io::export_workbook_full(
            path,
            &[ferrix_io::SheetExport::new("Sheet1", sheet)
                .with_formulas(&self.wb.overlay)
                .with_tables(&self.tables)
                // Cell decoration (issue #28) must survive the round trip for
                // the same reason protection does: a sheet the user bordered
                // and re-exported must still have its borders.
                .with_format(&self.wb.format)
                // Data validation must survive the round trip for the same
                // reason protection and decoration do (issue #41): a sheet
                // whose column the user restricted to a list, re-exported,
                // must still carry that list. This is the SAME chokepoint the
                // menu item runs, so a sibling export variant cannot silently
                // omit it.
                .with_validation(&self.wb.validation)
                // Sparklines (issue #36) survive as `<extLst>` groups. A group
                // Excel cannot express is reported below rather than silently
                // dropped.
                .with_sparklines(&self.wb.sparklines)
                // Dynamic-array spills (#27 P4): a spilling host is written as
                // `<f t="array" ref="...">` and its projections are left for
                // Excel to recompute, instead of freezing the array into a grid
                // of literals that would block its own re-spill on reopen.
                .with_spills(&self.wb.spills)
                .with_protection(self.wb.protection())],
            &self.wb.names,
            self.wb.workbook_protection(),
        ) {
            Ok(()) => {
                // Report what will NOT look the same in Excel, the way
                // `rule_survives_xlsx` does — the user learns here rather
                // than after opening the file.
                let lossy = self.decor_export_warnings();
                let base = format!(
                    "Exported {} table(s), {} name(s) → {}",
                    self.tables.len(),
                    self.wb.names.len(),
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                match lossy.first() {
                    None => base,
                    Some(first) if lossy.len() == 1 => format!("{base} — note: {first}"),
                    Some(first) => format!(
                        "{base} — note: {first} (and {} more formatting caveat(s))",
                        lossy.len() - 1
                    ),
                }
            }
            Err(e) => format!("Export failed: {e}"),
        };
    }

    /// Set the print area to the current selection (#37). A later print — PDF,
    /// HTML — renders only this range instead of the whole used extent. Stored
    /// once for the sheet, not per cell, so it costs nothing on a 200M-row
    /// sheet.
    pub fn set_print_area(&mut self) {
        let range = self.selection_range();
        self.print_area = Some(range);
        self.status = format!(
            "Print area set to {}{}:{}{}",
            ferrix_core::column_name(range.first_col),
            range.first_row + 1,
            ferrix_core::column_name(range.last_col),
            range.last_row + 1,
        );
    }

    /// Clear the print area, so the next print covers the whole sheet again.
    pub fn clear_print_area(&mut self) {
        if self.print_area.take().is_some() {
            self.status = "Print area cleared — prints will cover the whole sheet".into();
        } else {
            self.status = "No print area was set".into();
        }
    }

    /// The current print area, for tests and the menu's enabled state.
    pub fn print_area(&self) -> Option<ferrix_core::TableRange> {
        self.print_area
    }

    /// Toggle Page Break Preview (#37): the grid overlays a dashed line at
    /// every row and column where a printed page would break, so the user can
    /// see the pagination before printing. Read-only — no dragging.
    pub fn toggle_page_breaks(&mut self) {
        self.show_page_breaks = !self.show_page_breaks;
        self.status = if self.show_page_breaks {
            "Page Break Preview on — dashed lines show where pages split".into()
        } else {
            "Page Break Preview off".into()
        };
    }

    pub fn page_breaks_shown(&self) -> bool {
        self.show_page_breaks
    }

    /// Whether the app drove a continuous repaint on the last frame. True only
    /// for an in-content drag; false when idle or during an OS window move —
    /// the witness for the window-move-jitter guard (#84).
    pub fn is_driving_continuous_repaint(&self) -> bool {
        self.dragging_content
    }

    /// Insert a manual page break at the cursor: a horizontal break above the
    /// cursor's row and a vertical break to its left, so a page starts at the
    /// cursor. Excel's "Insert Page Break". A break before row/col 0 is
    /// meaningless (nothing is above/left of it) and is skipped. Turns the
    /// preview on so the user sees what changed.
    pub fn insert_page_break_at_cursor(&mut self) {
        let cell = self.selection.cursor;
        let mut added = Vec::new();
        if cell.row > 0 {
            self.page_setup.add_row_break(cell.row);
            added.push(format!("above row {}", cell.row + 1));
        }
        if cell.col > 0 {
            self.page_setup.add_col_break(cell.col);
            added.push(format!("left of {}", ferrix_core::column_name(cell.col)));
        }
        self.show_page_breaks = true;
        self.status = if added.is_empty() {
            "A page break at A1 would have nothing before it".into()
        } else {
            format!("Page break inserted {}", added.join(" and "))
        };
    }

    /// Remove the manual breaks at the cursor (the twin of insert). Reports
    /// whether anything was actually there to remove.
    pub fn remove_page_break_at_cursor(&mut self) {
        let cell = self.selection.cursor;
        let removed_row = self.page_setup.remove_row_break(cell.row);
        let removed_col = self.page_setup.remove_col_break(cell.col);
        self.status = if removed_row || removed_col {
            "Manual page break removed".into()
        } else {
            "No manual page break at the cursor".into()
        };
    }

    /// Clear every manual page break, so pagination is purely automatic again.
    pub fn reset_page_breaks(&mut self) {
        let had = !self.page_setup.row_breaks.is_empty() || !self.page_setup.col_breaks.is_empty();
        self.page_setup.row_breaks.clear();
        self.page_setup.col_breaks.clear();
        self.status = if had {
            "All manual page breaks reset".into()
        } else {
            "There were no manual page breaks".into()
        };
    }

    /// Move a manual page break from one row/column to another (#76). This is
    /// the model behind the drag: the gesture supplies `from`/`to`, this owns
    /// the meaning. Dropping a break on row/col 0, or back where it started, is
    /// a no-op. Returns whether the break set actually changed.
    ///
    /// Testable without a pointer: a harness drives `from`/`to` directly, so a
    /// broken drag gesture is the only thing left needing a human to confirm.
    pub fn move_row_break(&mut self, from: u32, to: u32) -> bool {
        // Remove the old break (whether or not it existed) and add the new one,
        // unless the target is the meaningless row 0.
        let removed = self.page_setup.remove_row_break(from);
        if to == 0 {
            // Dragged off the top: the break is deleted, not recreated.
            if removed {
                self.status = "Page break removed".into();
            }
            return removed;
        }
        if to == from {
            // Dropped back where it started: restore and report no change.
            if removed {
                self.page_setup.add_row_break(from);
            }
            return false;
        }
        self.page_setup.add_row_break(to);
        self.status = format!("Page break moved to above row {}", to + 1);
        true
    }

    /// Move a manual column break. Mirror of [`move_row_break`].
    pub fn move_col_break(&mut self, from: u32, to: u32) -> bool {
        let removed = self.page_setup.remove_col_break(from);
        if to == 0 {
            if removed {
                self.status = "Page break removed".into();
            }
            return removed;
        }
        if to == from {
            if removed {
                self.page_setup.add_col_break(from);
            }
            return false;
        }
        self.page_setup.add_col_break(to);
        self.status = format!(
            "Page break moved to left of {}",
            ferrix_core::column_name(to)
        );
        true
    }

    /// The sheet's manual page breaks (rows, cols), for tests.
    pub fn manual_page_breaks(&self) -> (&[u32], &[u32]) {
        (&self.page_setup.row_breaks, &self.page_setup.col_breaks)
    }

    /// Drive the page-break drag from the pointer (#76).
    ///
    /// Only MANUAL breaks are draggable — an automatic break has no stored
    /// position to move. On press near a manual break line (within
    /// `BREAK_GRAB_PX`), record which break was grabbed; on release, map the
    /// pointer to a row/column with `cell_at_point` and move the break there.
    /// The gesture only supplies coordinates; `move_row_break`/`move_col_break`
    /// own the meaning and are unit-tested without a pointer.
    fn handle_break_drag(
        &mut self,
        ctx: &egui::Context,
        outer: egui::Rect,
        row_lines: &[(f32, u32)],
        col_lines: &[(f32, u32)],
    ) {
        const BREAK_GRAB_PX: f32 = 4.0;
        let (pressed, down, released, pointer) = ctx.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.pointer.interact_pos(),
            )
        });
        let manual_rows = &self.page_setup.row_breaks;
        let manual_cols = &self.page_setup.col_breaks;

        // Press: grab the nearest manual break line under the pointer.
        if pressed {
            if let Some(p) = pointer {
                if outer.contains(p) {
                    // Row break lines are horizontal → compare the pointer's y.
                    let grabbed_row = row_lines
                        .iter()
                        .filter(|(_, r)| manual_rows.binary_search(r).is_ok())
                        .find(|(y, _)| (p.y - y).abs() <= BREAK_GRAB_PX)
                        .map(|(_, r)| *r);
                    let grabbed_col = col_lines
                        .iter()
                        .filter(|(_, c)| manual_cols.binary_search(c).is_ok())
                        .find(|(x, _)| (p.x - x).abs() <= BREAK_GRAB_PX)
                        .map(|(_, c)| *c);
                    // A row grab wins ties (horizontal lines are the common
                    // case); either sets the drag in flight.
                    if let Some(r) = grabbed_row {
                        self.break_drag = Some(BreakDrag {
                            axis: BreakAxis::Row,
                            origin: r,
                        });
                    } else if let Some(c) = grabbed_col {
                        self.break_drag = Some(BreakDrag {
                            axis: BreakAxis::Col,
                            origin: c,
                        });
                    }
                }
            }
        }

        // Release: drop the grabbed break at the row/col under the pointer.
        if released {
            if let Some(drag) = self.break_drag.take() {
                if let Some(cell) = pointer.and_then(|p| self.cell_at_point(p, outer)) {
                    match drag.axis {
                        BreakAxis::Row => {
                            self.move_row_break(drag.origin, cell.row);
                        }
                        BreakAxis::Col => {
                            self.move_col_break(drag.origin, cell.col);
                        }
                    }
                }
                // A release outside any cell drops the drag with no change.
            }
        } else if !down {
            // Pointer lifted without a release event reaching us (focus lost):
            // never leave a stale drag armed.
            self.break_drag = None;
        }
    }

    /// How many page-break lines the last frame drew, for tests.
    pub fn page_break_line_count(&self) -> usize {
        self.last_page_break_lines
    }

    /// Write this sheet to `path` as a PDF or a single-file HTML page.
    ///
    /// Dialog-free so a test can drive the REAL export the menu runs (the
    /// `export_xlsx_to` convention): testing `render_pdf` directly would pass
    /// even if this method wired the wrong sheet, name, or sizing. Runs on the
    /// calling thread like the Parquet export — the write itself streams one
    /// page at a time, so the memory bound holds; only UI responsiveness during
    /// a very large forced job does not, which the large-job refusal makes a
    /// deliberate choice rather than a silent freeze.
    ///
    /// `confirm_large` is threaded through: the first call refuses a job over
    /// [`ferrix_core::page::LARGE_JOB_PAGES`] pages and reports the count, so a
    /// user asking to print a 200M-row sheet is warned instead of handed a
    /// 200,000-page file. A caller that meant it passes `confirm_large = true`.
    pub fn print_to_path(&mut self, path: &std::path::Path, html: bool, confirm_large: bool) {
        if self.exporting {
            self.status = "An export is already running — cancel it first".into();
            return;
        }
        let cost = crate::sheet_view::OwnedSheet::snapshot_cost_bytes(&self.wb.overlay)
            + crate::sheet_view::OwnedSheet::style_cost_bytes(&self.wb.format, &self.wb.merges);
        if let Err(msg) = ferrix_core::Budget::sample().admit(cost, "Printing this sheet's edits") {
            self.status = msg;
            return;
        }

        let name = self.active_sheet_name().to_string();
        let snapshot = crate::sheet_view::OwnedSheet::new(
            std::sync::Arc::clone(&self.wb.base),
            &self.wb.overlay,
        )
        .with_name(&name)
        .with_style(&self.wb.format, &self.wb.merges);

        // The sheet's real page setup, so any manual page breaks the user set
        // in the preview land in the printed output.
        let setup = self.page_setup.clone();
        let opts = ferrix_io::render::RenderOptions {
            print_area: self.print_area,
            ..Default::default()
        };
        let rows = self.sizing.rows.clone();
        let cols = self.sizing.cols.clone();

        let result = if html {
            ferrix_io::render::render_html(
                path,
                &snapshot,
                &setup,
                &opts,
                &rows,
                &cols,
                confirm_large,
                |_, _| {},
                || false,
            )
        } else {
            ferrix_io::render::render_pdf(
                path,
                &snapshot,
                &setup,
                &opts,
                &rows,
                &cols,
                confirm_large,
                |_, _| {},
                || false,
            )
        };

        self.status = match result {
            Ok(stats) => format!(
                "Printed {} page(s), {} row(s) ({:.1} MB) → {}",
                fmt_int(stats.pages as usize),
                fmt_int(stats.rows as usize),
                stats.bytes as f64 / 1e6,
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Err(ferrix_io::render::RenderError::TooManyPages(n)) => format!(
                "This would print {} pages — nothing was written. Choose a print area or \
                 confirm to proceed.",
                fmt_int(n as usize)
            ),
            Err(e) => format!("Print failed: {e}"),
        };
    }

    /// Menu entry: pick a PDF path, then print to it.
    fn print_pdf_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF", &["pdf"])
            .set_file_name("print.pdf")
            .save_file()
        else {
            return;
        };
        // First attempt refuses a large job; the user re-invokes to confirm.
        self.print_to_path(&path, false, false);
    }

    /// Menu entry: pick an HTML path, then print to it.
    fn print_html_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("HTML", &["html"])
            .set_file_name("print.html")
            .save_file()
        else {
            return;
        };
        self.print_to_path(&path, true, false);
    }
    ///
    /// Public because the export dialog wants it BEFORE writing, not only in
    /// the status line afterwards.
    pub fn decor_export_warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push = |d: &ferrix_core::CellDecor, out: &mut Vec<String>| {
            for m in ferrix_io::decor_xlsx_loss(d) {
                if !out.contains(&m) {
                    out.push(m);
                }
            }
        };
        for (_, cf) in self.wb.format.columns() {
            push(&cf.decor, &mut out);
        }
        for rf in self.wb.format.ranges() {
            push(&rf.decor, &mut out);
        }
        for (_, ov) in self.wb.format.overrides() {
            push(&ov.decor, &mut out);
        }
        // Sparklines (issue #36), same contract: a group Excel cannot express
        // is reported HERE, in the editor, rather than discovered after the
        // file is opened.
        for m in ferrix_io::sparkline_xlsx_loss(&self.wb.sparklines) {
            if !out.contains(&m) {
                out.push(m);
            }
        }
        out
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

    /// What the formula bar would show for a cell: a formula's SOURCE text,
    /// otherwise its display value.
    ///
    /// This is the only way a test can tell "the formula was rewritten" from
    /// "the formula was replaced by a literal that happens to show the same
    /// number" — the exact confusion 'look in: formulas' has to avoid.
    pub fn edit_text(&self, cell: CellRef) -> String {
        self.wb.view().edit_text(cell)
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

    /// What the grid would draw in a cell — base with the overlay applied.
    /// For the headless harness.
    pub fn display_for_test(&self, cell: CellRef) -> String {
        self.wb.view().display(cell)
    }

    /// Write the sidecar, for the headless harness. Same entry point the
    /// toolbar's Save button and Ctrl+S use.
    pub fn save_edits_for_test(&mut self) -> bool {
        self.save_edits()
    }

    // --- Name Box / Name Manager seams ---
    //
    // The Name Box is a TextEdit whose pixel position depends on theme text
    // metrics; synthesising a click on it would test layout rather than the
    // feature. These drive the SAME state the widget writes and the SAME
    // entry point Enter calls, so a test asserts on real behaviour.

    /// Type into the Name Box, as the widget's `changed()` branch does.
    pub fn type_in_name_box(&mut self, text: &str) {
        self.name_box_edit = Some(text.to_string());
    }

    /// Scope newly-defined names to the active sheet rather than the workbook.
    pub fn set_name_box_sheet_scope(&mut self, on: bool) {
        self.name_box_sheet_scope = on;
    }

    /// Is the Name Manager window on screen?
    pub fn names_manager_open(&self) -> bool {
        self.names_open
    }

    pub fn open_name_manager(&mut self) {
        self.names_open = true;
    }

    /// Begin editing a name in the manager, as its Edit button does.
    pub fn begin_name_edit(&mut self, ident: &str, scope: ferrix_formula::NameScope) {
        let refers_to = self
            .wb
            .names
            .get_scoped(ident, &scope)
            .map(|d| d.refers_to.clone())
            .unwrap_or_default();
        self.name_edit_ident = ident.to_string();
        self.name_edit_target = refers_to;
        self.name_edit = Some((ident.to_string(), scope));
        self.name_error = None;
    }

    /// Set the manager's identifier buffer, as typing into it does.
    pub fn set_name_edit_ident(&mut self, ident: &str) {
        self.name_edit_ident = ident.to_string();
    }

    /// Set the manager's "refers to" buffer.
    pub fn set_name_edit_target(&mut self, target: &str) {
        self.name_edit_target = target.to_string();
    }

    /// The manager's Apply button.
    pub fn apply_name_edit_now(&mut self) {
        self.apply_name_edit();
    }

    /// The manager's Delete button.
    pub fn delete_name_now(&mut self, ident: &str, scope: &ferrix_formula::NameScope) {
        self.delete_name_ui(ident, scope);
    }

    /// The manager's last reported error, if any.
    pub fn name_error_text(&self) -> Option<&str> {
        self.name_error.as_deref()
    }

    /// Read-only access to the workbook, for tests that assert on names.
    pub fn workbook(&self) -> &Workbook {
        &self.wb
    }

    /// Mutable workbook access, for tests that seed a store directly.
    ///
    /// Used to place a validation rule without driving the whole dialog, so a
    /// test about the EDIT PATH is not also a test about the dialog. The tests
    /// that assert the dialog works drive the dialog.
    pub fn workbook_mut(&mut self) -> &mut Workbook {
        &mut self.wb
    }

    /// The text currently in the CELL editor, or `None` when no edit is live.
    ///
    /// The accessor "Escape did not alter the typed text" is asserted on.
    pub fn live_edit_buffer(&self) -> Option<String> {
        self.editing.is_some().then(|| self.edit_buffer.clone())
    }

    /// Where the in-cell dropdown arrow was PAINTED last frame, if anywhere.
    ///
    /// Real paint geometry from the grid's own loop, so a test asserting the
    /// arrow exists is reading the screen rather than the model.
    pub fn dropdown_button_rect(&self) -> Option<egui::Rect> {
        self.dropdown_button.map(|(_, r)| r)
    }

    /// The active selection, so a navigation can be checked.
    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// Type into the formula bar, as the TextEdit's `changed()` branch does.
    pub fn set_formula_input(&mut self, text: &str) {
        self.formula_input = text.to_string();
        self.recompute_formula();
    }

    /// The formula bar's live preview of what the typed formula evaluates to.
    pub fn formula_preview(&self) -> Option<&str> {
        self.formula_result.as_deref()
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

    /// Move rows as a display-order permutation (issue #17, scope item 4).
    ///
    /// A row MOVE, not an arbitrary permutation: see `Workbook::move_rows` for
    /// why that is affordable at 200M rows and what the visible limit is. A
    /// refusal lands in the status line rather than being swallowed.
    pub fn move_rows(&mut self, from: u64, count: u64, to: u64) -> Result<(), String> {
        let r = self.wb.move_rows(from, count, to);
        match &r {
            Ok(()) => {
                let (runs, cap) = self.wb.row_order_runs();
                // The run count is surfaced as it approaches the cap, so the
                // user meets the limit as a warning rather than as a refusal
                // out of nowhere.
                self.status = if runs * 4 > cap {
                    format!(
                        "Moved {count} row(s) — {runs} of {cap} reorder steps tracked; \
                         save and reopen to start from a clean order"
                    )
                } else {
                    format!("Moved {count} row(s) to row {}", to + 1)
                };
            }
            Err(e) => self.status = format!("Cannot move those rows: {e}"),
        }
        r
    }

    // ---- structural edits: insert / delete row and column (issue #17) ----
    //
    // Each takes the span from the CURRENT SELECTION, so "Insert Row" with
    // three rows selected inserts three — the behaviour every spreadsheet has.
    // Each is one undo step, and each reports what it did in the status line
    // so a refusal (see `AxisOrder::MAX_RUNS`) is visible rather than silent.

    /// Rows the selection spans, as `(first, count)` in display space.
    fn selected_row_span(&self) -> (u64, u64) {
        let (a, b) = self.selection.row_range();
        (u64::from(a), u64::from(b - a + 1))
    }

    /// Columns the selection spans, as `(first, count)` in display space.
    fn selected_col_span(&self) -> (u64, u64) {
        let (a, b) = self.selection.col_range();
        (u64::from(a), u64::from(b - a + 1))
    }

    pub fn insert_rows_at_selection(&mut self) {
        let (at, count) = self.selected_row_span();
        let outcome = self
            .wb
            .insert_rows(at, count)
            .map(|()| format!("Inserted {count} row(s) at row {}", at + 1));
        self.apply_structural(outcome);
    }

    pub fn delete_rows_at_selection(&mut self) {
        let (at, count) = self.selected_row_span();
        let outcome = self
            .wb
            .delete_rows(at, count)
            .map(|()| format!("Deleted {count} row(s) from row {}", at + 1));
        self.apply_structural(outcome);
    }

    pub fn insert_columns_at_selection(&mut self) {
        let (at, count) = self.selected_col_span();
        let outcome = self.wb.insert_columns(at, count).map(|()| {
            format!(
                "Inserted {count} column(s) at {}",
                ferrix_core::column_name(at as u32)
            )
        });
        self.apply_structural(outcome);
    }

    pub fn delete_columns_at_selection(&mut self) {
        let (at, count) = self.selected_col_span();
        let outcome = self.wb.delete_columns(at, count).map(|()| {
            format!(
                "Deleted {count} column(s) from {}",
                ferrix_core::column_name(at as u32)
            )
        });
        self.apply_structural(outcome);
    }

    /// Report a structural edit and refresh the derived view state.
    ///
    /// A REFUSAL IS SHOWN, not swallowed. `AxisOrder` refuses an edit that
    /// would fragment the display order past its cap, and the whole point of
    /// that cap is that the user sees the limit rather than feeling it as
    /// unexplained slowness.
    fn apply_structural(&mut self, outcome: Result<String, String>) {
        match outcome {
            Ok(msg) => {
                self.status = msg;
                // The sheet's extent changed, so anything derived from it —
                // the row count in the status bar, the table filter mask —
                // has to be recomputed rather than left describing the old
                // shape.
                let rows = self.wb.view().row_count();
                self.stats_rows = rows;
                self.refresh_tables();
                self.clamp_selection_to_sheet();
                self.sync_formula_bar();
            }
            Err(e) => self.status = format!("Cannot do that: {e}"),
        }
    }

    /// Pull the selection back inside the sheet after a delete shrinks it.
    ///
    /// Without this, deleting the last row leaves the cursor addressing a row
    /// that no longer exists, and the next keystroke would extend the sheet to
    /// recreate it.
    fn clamp_selection_to_sheet(&mut self) {
        let view = self.wb.view();
        let last_row = view.row_count().saturating_sub(1) as u32;
        let last_col = view.col_count().saturating_sub(1) as u32;
        let clamp = |c: CellRef| CellRef::new(c.row.min(last_row), c.col.min(last_col));
        self.selection = Selection::new(clamp(self.selection.anchor), clamp(self.selection.cursor));
    }

    // ---- row / column header selection (issue #17) ----
    //
    // The COLUMN case existed (press selects the whole column); the ROW case
    // did not, and neither had Ctrl for disjoint or Shift for a span. All
    // three go through one pair of methods so a row and a column cannot end up
    // behaving differently by accident.

    /// Select the whole of display row `row`.
    ///
    /// `mods` decides how it composes with what is already selected:
    /// * plain — replace the selection with this row;
    /// * Shift — extend from the anchor to cover every row between;
    /// * Ctrl  — ADD this row as a disjoint range, leaving the others alone.
    ///
    /// The selection stays two corners in every case. Selecting row 1 and row
    /// 50,000,000 of a 200M-row sheet must not materialise the 50M rows
    /// between them, which is exactly what a bounding-box-only model would do
    /// and why Ctrl needs its own list.
    pub fn select_row(&mut self, row: u32, mods: egui::Modifiers) {
        let last_col = self.stats_cols.saturating_sub(1) as u32;
        let band = Selection::new(CellRef::new(row, 0), CellRef::new(row, last_col));
        let note = self.apply_header_selection(band, mods, true);
        // A warning outranks the routine confirmation: "row 4 selected" is
        // what the user can already see, whereas "the oldest was dropped" is
        // the only signal that the cap just bit.
        self.status = note.unwrap_or_else(|| format!("Row {} selected", row as u64 + 1));
    }

    /// Select the whole of display column `col`. Same modifier rules as
    /// [`Self::select_row`].
    pub fn select_column(&mut self, col: u32, mods: egui::Modifiers) {
        let last_row = self.stats_rows.saturating_sub(1) as u32;
        let band = Selection::new(CellRef::new(0, col), CellRef::new(last_row, col));
        let note = self.apply_header_selection(band, mods, false);
        self.status =
            note.unwrap_or_else(|| format!("Column {} selected", ferrix_core::column_name(col)));
    }

    /// Compose a header band with the existing selection per the modifiers.
    ///
    /// Returns a status message the caller must PREFER over its own, when the
    /// composition did something the user needs told about.
    fn apply_header_selection(
        &mut self,
        band: Selection,
        mods: egui::Modifiers,
        is_row: bool,
    ) -> Option<String> {
        let mut note = None;
        if mods.command {
            // Ctrl: a DISJOINT addition. The current selection is pushed into
            // the extra list and the new band becomes the active one, so the
            // cursor — and therefore where typing lands — is always the band
            // the user just clicked.
            if self.extra_selections.len() < MAX_DISJOINT_SELECTIONS {
                self.extra_selections.push(self.selection);
            } else {
                // Visible, not silent. A cap the user cannot see is a cap they
                // experience as the feature randomly not working.
                note = Some(format!(
                    "Only {MAX_DISJOINT_SELECTIONS} separate selections are kept — \
                     the oldest was dropped"
                ));
                self.extra_selections.remove(0);
                self.extra_selections.push(self.selection);
            }
            self.selection = band;
        } else if mods.shift {
            // Shift: one contiguous span from the existing ANCHOR to this
            // band. Disjoint ranges are cleared, matching what every
            // spreadsheet does — a span replaces a scattering.
            self.extra_selections.clear();
            let anchor = self.selection.anchor;
            self.selection = if is_row {
                Selection::new(
                    CellRef::new(anchor.row, band.anchor.col),
                    CellRef::new(band.cursor.row, band.cursor.col),
                )
            } else {
                Selection::new(
                    CellRef::new(band.anchor.row, anchor.col),
                    CellRef::new(band.cursor.row, band.cursor.col),
                )
            };
        } else {
            self.extra_selections.clear();
            self.selection = band;
        }
        self.focus = Focus::Grid;
        self.sync_formula_bar();
        note
    }

    /// The disjoint ranges currently held, for tests and for the paint call.
    pub fn extra_selections(&self) -> &[Selection] {
        &self.extra_selections
    }

    /// Whether a cell is inside ANY selected range, active or disjoint.
    pub fn cell_is_selected(&self, cell: CellRef) -> bool {
        self.selection.contains(cell) || self.extra_selections.iter().any(|s| s.contains(cell))
    }

    /// Whether a display row is fully or partly selected, counting disjoint
    /// ranges. This is what the row header highlight reads.
    pub fn row_is_selected(&self, row: u32) -> bool {
        let hits = |s: &Selection| {
            let (a, b) = s.row_range();
            row >= a && row <= b
        };
        hits(&self.selection) || self.extra_selections.iter().any(hits)
    }

    /// Whether a display column is fully or partly selected, counting disjoint
    /// ranges.
    pub fn column_is_selected(&self, col: u32) -> bool {
        let hits = |s: &Selection| {
            let (a, b) = s.col_range();
            col >= a && col <= b
        };
        hits(&self.selection) || self.extra_selections.iter().any(hits)
    }

    /// Where each visible ROW header was painted last frame, for tests that
    /// need to click one without guessing pixels.
    pub fn row_header_center(&self, row: u32) -> Option<(f32, f32)> {
        self.row_header_hitboxes
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, p)| (p.x, p.y))
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

    // ------------------------------------------- formula bar upgrades (#38)

    /// Ctrl+` — show formula SOURCE instead of values, for THIS sheet.
    ///
    /// Per sheet rather than global because it is a way of looking at one
    /// sheet: flipping to a lookup table to read a number should not require
    /// turning the mode off and back on again.
    ///
    /// Nothing is precomputed here. The set holds sheet ids, and the grid
    /// fetches each visible cell's source in its paint loop — so turning this
    /// on over a 200M-row sheet allocates a viewport of strings, not a sheet
    /// of them.
    pub fn toggle_show_formulas(&mut self) {
        let id = self.wb.active_sheet();
        let on = if self.show_formulas.contains(&id) {
            self.show_formulas.remove(&id);
            false
        } else {
            self.show_formulas.insert(id);
            true
        };
        self.status = format!(
            "{}: showing {}",
            self.wb.active_name(),
            if on { "formulas" } else { "values" }
        );
    }

    /// Whether the ACTIVE sheet is in show-formulas mode.
    pub fn showing_formulas(&self) -> bool {
        self.show_formulas.contains(&self.wb.active_sheet())
    }

    /// Whether a named sheet is, for the per-sheet assertion.
    pub fn showing_formulas_on(&self, id: ferrix_core::SheetId) -> bool {
        self.show_formulas.contains(&id)
    }

    /// How many text rows tall the formula bar is.
    pub fn formula_bar_rows(&self) -> usize {
        self.formula_bar_rows
    }

    /// The height the formula bar panel actually occupied last frame.
    ///
    /// Real layout output rather than `rows * something`, so a test can tell
    /// "the number changed" apart from "the bar grew".
    pub fn formula_bar_height(&self) -> f32 {
        self.last_formula_bar_h
    }

    /// Resize the formula bar. The entry point the drag handle calls, and the
    /// one that persists the choice.
    pub fn set_formula_bar_rows(&mut self, rows: usize) {
        let rows = rows.clamp(
            crate::prefs::MIN_FORMULA_BAR_ROWS,
            crate::prefs::MAX_FORMULA_BAR_ROWS,
        );
        if rows == self.formula_bar_rows {
            return;
        }
        self.formula_bar_rows = rows;
        self.prefs.formula_bar_rows = rows;
        // Persisted on the change, not at shutdown: a preference that only
        // survives a clean exit does not survive a crash, and this one is
        // cheap to write.
        self.persist_prefs();
    }

    /// The formula bar's live text, for assertions.
    pub fn formula_bar_text(&self) -> &str {
        &self.formula_input
    }

    /// Commit the formula bar to the cursor cell — the same call the bar's
    /// Enter branch makes.
    ///
    /// Exposed because the alternative is synthesising a click into a text
    /// field and an Enter whose focus behaviour is egui's, which would test
    /// widget focus rather than what a committed formula does.
    pub fn commit_formula_bar_for_test(&mut self) {
        let cell = self.selection.cursor;
        let text = self.formula_input.clone();
        self.wb.commit_edit(cell, &text);
        self.sync_formula_bar();
    }

    /// Start an edit, as typing / F2 / double-click all do, through the one
    /// chokepoint they share.
    pub fn begin_edit_for_test(&mut self, cell: CellRef, seed: Option<&str>) {
        self.selection.move_to(cell);
        self.begin_edit(cell, seed.map(str::to_string));
    }

    /// Abandon an edit, as Escape does.
    pub fn cancel_edit_for_test(&mut self) {
        self.cancel_edit();
    }

    /// Put app-level focus in the formula bar and take the Escape snapshot,
    /// as the widget's `gained_focus()` branch does.
    pub fn focus_formula_bar_for_test(&mut self) {
        self.focus = Focus::FormulaBar;
        self.edit_pre_text.clone_from(&self.formula_input);
    }

    /// Add and switch to a fresh sheet, as the tab bar's + button does.
    pub fn add_sheet_for_test(&mut self) {
        self.add_sheet();
    }

    /// Leave the start screen without choosing anything, so a test can see
    /// the empty-state grid the cold-start path paints behind it.
    pub fn dismiss_start_screen_for_test(&mut self) {
        self.show_start = false;
    }

    /// Switch to a sheet by tab position, as clicking its tab does.
    ///
    /// Goes through `switch_sheet` rather than straight to the workbook, so a
    /// test lands in the same state a real tab click leaves the app in — with
    /// the view transforms rebuilt for the sheet that is now showing.
    pub fn switch_to_sheet_for_test(&mut self, index: usize) -> bool {
        let Some(&(id, _)) = self
            .wb
            .sheet_names()
            .iter()
            .collect::<Vec<_>>()
            .get(index)
            .copied()
        else {
            return false;
        };
        self.switch_sheet(id);
        true
    }

    /// Caret position in the live editor, in bytes.
    pub fn edit_caret(&self) -> usize {
        self.edit_caret
    }

    /// Move the caret in the live editor, in bytes. Used by tests to park on
    /// a particular reference before pressing F4; the running app gets this
    /// from egui.
    pub fn set_edit_caret(&mut self, byte: usize) {
        self.edit_caret = byte;
        self.pending_caret = Some(byte);
    }

    /// F4: cycle the anchoring of the reference under the caret.
    ///
    /// Rewrites the formula TEXT through `refedit`, which splices the one
    /// reference's bytes and copies everything else verbatim. Going through
    /// the parser here would re-render every reference in the formula and drop
    /// the `$` markers on the ones the user did not touch — see the module
    /// docs on `ferrix_formula::refedit`.
    pub fn cycle_reference_anchor(&mut self) -> bool {
        let Some((is_cell, text)) = self.live_edit_text() else {
            return false;
        };
        let caret = self.edit_caret.min(text.len());
        // Byte offsets from egui are always on a char boundary, but a caret
        // restored from a stale frame need not be — clamp rather than panic.
        let caret = (0..=caret)
            .rev()
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(0);
        let Some((next, span)) = ferrix_formula::refedit::cycle_at(&text, caret) else {
            self.status = "F4: no cell reference under the cursor".into();
            return false;
        };
        let shown = next[span.clone()].to_string();
        self.set_live_edit_text(is_cell, next);
        self.edit_caret = span.end;
        self.pending_caret = Some(span.end);
        self.status = format!("F4: {shown}");
        true
    }

    /// The reference spans in whatever is being edited, in source order.
    ///
    /// Empty when nothing is being edited, which is what makes the outlines
    /// disappear the moment the edit ends.
    fn live_ref_spans(&self) -> Vec<ferrix_formula::refedit::RefSpan> {
        match self.live_edit_text() {
            // Only a FORMULA has references. A cell holding the literal text
            // `A1` must not sprout an outline.
            Some((_, t)) if t.starts_with('=') => ferrix_formula::refedit::spans(&t),
            _ => Vec::new(),
        }
    }

    /// Reference outlines painted by the last frame: `(rect, colour)`.
    ///
    /// Recorded at the point of painting, so a test reading this is reading
    /// the screen. An outline that is computed but never drawn is absent here.
    pub fn ref_outlines(&self) -> &[(egui::Rect, egui::Color32)] {
        &self.ref_outlines
    }

    /// Move the reference at `span` by a whole-cell offset, as dropping its
    /// dragged outline does. Returns false when the move was refused.
    fn move_reference(&mut self, span_index: usize, d_row: i64, d_col: i64) -> bool {
        if d_row == 0 && d_col == 0 {
            return false;
        }
        let Some((is_cell, text)) = self.live_edit_text() else {
            return false;
        };
        let spans = ferrix_formula::refedit::spans(&text);
        let Some(span) = spans.get(span_index) else {
            return false;
        };
        let Some(next) = ferrix_formula::refedit::shift_span(&text, span, d_row, d_col) else {
            self.status = "That would move the reference off the sheet".into();
            return false;
        };
        let label = next[span.start..]
            .split(['+', '-', '*', '/', ')', ','])
            .next()
            .unwrap_or("");
        self.status = format!("Reference moved to {}", label.trim());
        self.set_live_edit_text(is_cell, next);
        true
    }

    // ------------------------------------------------------------ Name Box

    /// What the Name Box shows: the selection's defined name if it has one,
    /// otherwise its A1 label.
    ///
    /// Read live from the workbook rather than cached, so defining, renaming
    /// or deleting a name is reflected without any explicit refresh.
    pub fn name_box_text(&self) -> String {
        if let Some(buf) = &self.name_box_edit {
            return buf.clone();
        }
        match self.wb.name_for_selection(self.selection) {
            Some(n) => n.to_string(),
            None => self.selection.label(),
        }
    }

    /// Commit whatever is typed in the Name Box.
    ///
    /// An EXISTING name navigates to it (switching sheets when it lives
    /// elsewhere); an A1 reference or range navigates there; anything else
    /// valid DEFINES a new name for the current selection. Excel's Name Box
    /// behaves the same way, and it is what makes naming a range a one-gesture
    /// operation rather than a trip through a dialog.
    pub fn commit_name_box(&mut self) {
        let Some(text) = self.name_box_edit.take() else {
            return;
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        // 1. An existing name: navigate.
        if let Some((sheet, target)) = self.wb.name_target(&text) {
            if sheet != self.wb.active_sheet() {
                self.switch_sheet(sheet);
            }
            self.set_selection(target);
            self.status = format!("Went to {text}");
            return;
        }

        // 2. A literal address: navigate there too, so the box doubles as
        //    "go to cell" the way every spreadsheet's does.
        if let Some(sel) = parse_a1_selection(&text) {
            self.set_selection(sel);
            self.status = format!("Went to {}", sel.label());
            return;
        }

        // 3. Otherwise define it for the current selection.
        let scope = if self.name_box_sheet_scope {
            ferrix_formula::NameScope::Sheet(self.wb.active_name().to_string())
        } else {
            ferrix_formula::NameScope::Workbook
        };
        let sel = self.selection;
        self.status = match self.wb.define_name(&text, scope, sel) {
            Ok(()) => format!("Defined {text} = {}", sel.label()),
            Err(e) => format!("Cannot define {text:?}: {e}"),
        };
    }

    /// Move the cursor and scroll to a target selection.
    fn set_selection(&mut self, sel: Selection) {
        self.selection = sel;
        self.center_on_selection();
        self.sync_formula_bar();
    }

    /// Delete a name through the manager, reporting what happened.
    fn delete_name_ui(&mut self, ident: &str, scope: &ferrix_formula::NameScope) {
        let affected = self.wb.graph.cells_using_name(ident).len();
        if self.wb.delete_name(ident, scope).is_some() {
            self.status = if affected > 0 {
                format!("Deleted {ident} — {affected} formula(s) now #NAME?")
            } else {
                format!("Deleted {ident}")
            };
        }
    }

    /// Apply the Name Manager's edit buffers to the entry being edited.
    fn apply_name_edit(&mut self) {
        let Some((orig, scope)) = self.name_edit.clone() else {
            return;
        };
        self.name_error = None;

        // Retarget first: if the identifier also changed, the rename below
        // finds the entry by its ORIGINAL name, which is still in place.
        let target = self.name_edit_target.trim().to_string();
        if !target.is_empty() {
            if let Err(e) = self.wb.retarget_name(&orig, &scope, &target) {
                self.name_error = Some(e.to_string());
                return;
            }
        }

        let ident = self.name_edit_ident.trim().to_string();
        if !ident.is_empty() && !ident.eq_ignore_ascii_case(&orig) {
            match self.wb.rename_name(&orig, &scope, &ident) {
                Ok(n) => {
                    self.status = format!("Renamed {orig} → {ident} · {n} formula(s) rewritten");
                }
                Err(e) => {
                    self.name_error = Some(e.to_string());
                    return;
                }
            }
        }
        self.name_edit = None;
    }

    /// The Name Manager: list every defined name, edit or delete each one.
    ///
    /// A modal window rather than a panel, because editing a name can rewrite
    /// formulas across the whole workbook and that should be a deliberate act,
    /// not something reachable by a stray click while typing in a cell.
    fn show_name_manager(&mut self, ctx: &egui::Context) {
        let th = self.theme;
        let mut open = self.names_open;
        // Actions are collected and applied after the closure, so the list is
        // never mutated while it is being iterated.
        let mut to_delete: Option<(String, ferrix_formula::NameScope)> = None;
        let mut to_edit: Option<(String, ferrix_formula::NameScope, String)> = None;
        let mut to_goto: Option<String> = None;
        let mut apply = false;

        egui::Window::new("Name Manager")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                let names: Vec<(String, ferrix_formula::NameScope, String)> = self
                    .wb
                    .names
                    .iter()
                    .map(|d| (d.name.clone(), d.scope.clone(), d.refers_to.clone()))
                    .collect();

                if names.is_empty() {
                    ui.label(
                        RichText::new(
                            "No defined names yet. Select a range and type a name into the \
                             Name Box to create one.",
                        )
                        .color(th.text_dim),
                    );
                    return;
                }

                egui::Grid::new("names_grid")
                    .num_columns(4)
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(RichText::new("Name").strong());
                        ui.label(RichText::new("Scope").strong());
                        ui.label(RichText::new("Refers to").strong());
                        ui.label("");
                        ui.end_row();

                        for (name, scope, refers_to) in &names {
                            let editing = self
                                .name_edit
                                .as_ref()
                                .is_some_and(|(n, s)| n == name && s == scope);
                            if editing {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.name_edit_ident)
                                        .desired_width(120.0),
                                );
                            } else {
                                ui.label(RichText::new(name).monospace().color(th.accent));
                            }

                            ui.label(
                                RichText::new(scope.sheet().unwrap_or("Workbook"))
                                    .color(th.text_dim),
                            );

                            if editing {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.name_edit_target)
                                        .font(egui::TextStyle::Monospace)
                                        .desired_width(220.0),
                                );
                            } else {
                                ui.label(RichText::new(refers_to).monospace());
                            }

                            ui.horizontal(|ui| {
                                if editing {
                                    if ui.button("Apply").clicked() {
                                        apply = true;
                                    }
                                    if ui.button("Cancel").clicked() {
                                        self.name_edit = None;
                                        self.name_error = None;
                                    }
                                } else {
                                    if ui.button("Edit").clicked() {
                                        to_edit =
                                            Some((name.clone(), scope.clone(), refers_to.clone()));
                                    }
                                    if ui.button("Go to").clicked() {
                                        to_goto = Some(name.clone());
                                    }
                                    // How many formulas a delete would break,
                                    // shown BEFORE the click rather than after.
                                    let uses = self.wb.graph.cells_using_name(name).len();
                                    let label = if uses > 0 {
                                        format!("Delete ({uses})")
                                    } else {
                                        "Delete".to_string()
                                    };
                                    let btn = ui.button(label);
                                    let btn = if uses > 0 {
                                        btn.on_hover_text(format!(
                                            "{uses} formula(s) will become #NAME?"
                                        ))
                                    } else {
                                        btn
                                    };
                                    if btn.clicked() {
                                        to_delete = Some((name.clone(), scope.clone()));
                                    }
                                }
                            });
                            ui.end_row();
                        }
                    });

                if let Some(e) = &self.name_error {
                    ui.separator();
                    ui.label(RichText::new(e).color(th.error));
                }
            });

        if apply {
            self.apply_name_edit();
        }
        if let Some((name, scope, refers_to)) = to_edit {
            self.name_edit_ident = name.clone();
            self.name_edit_target = refers_to;
            self.name_edit = Some((name, scope));
            self.name_error = None;
        }
        if let Some(name) = to_goto {
            if let Some((sheet, target)) = self.wb.name_target(&name) {
                if sheet != self.wb.active_sheet() {
                    self.switch_sheet(sheet);
                }
                self.set_selection(target);
            }
        }
        if let Some((name, scope)) = to_delete {
            self.delete_name_ui(&name, &scope);
            self.name_edit = None;
        }
        self.names_open = open;
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
        // Zoom and panes belong to the SHEET, not to the session: adopt the
        // new sheet's remembered zoom and drop the old sheet's frozen band,
        // which was defined in that sheet's row and column space.
        self.zoom = self.prefs.zoom_of(&self.book_key(), self.wb.active_name());
        self.panes = crate::grid::Panes::default();
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
                // A sheet the user created is a workbook in front of them,
                // empty or not — it must be typeable (issue #52).
                self.workbook_started = true;
                self.switch_sheet(id);
                self.status = format!("Added sheet {name}");
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    fn delete_sheet(&mut self, id: ferrix_core::SheetId) {
        let name = self.wb.sheet_name(id).unwrap_or("").to_string();
        match self.wb.delete_sheet(id) {
            Ok(broken) => {
                // Deleting may have changed which sheet is active.
                let state = self.wb.view_state();
                self.scroll = state.scroll;
                self.selection = state.selection;
                self.col_widths = Vec::new();
                let view = self.wb.view();
                self.stats_rows = view.row_count();
                self.stats_cols = view.col_count();
                // Say out loud how many formulas the delete broke. A silent
                // sheet full of fresh #REF! is exactly the surprise the
                // "report lossy operations" rule exists to prevent.
                self.status = match broken {
                    0 => format!("Deleted sheet {name}"),
                    1 => format!("Deleted sheet {name}; 1 formula now #REF!"),
                    n => format!("Deleted sheet {name}; {n} formulas now #REF!"),
                };
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
            Ok(rewritten) => {
                // Say how many formulas followed the rename. Silence would
                // leave the user unable to tell a rewrite from a no-op.
                self.status = match rewritten {
                    0 => format!("Renamed sheet to {}", name.trim()),
                    1 => format!("Renamed sheet to {}; 1 formula updated", name.trim()),
                    n => format!("Renamed sheet to {}; {n} formulas updated", name.trim()),
                };
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
            // A drag that lands nowhere must say why (issue #42): a tab that
            // silently springs back reads as a broken drag, not as a refusal.
            if let Err(e) = self.wb.reorder_sheet(id, to) {
                self.status = format!("Reorder refused — {e}");
            }
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
        let Some(query) = self.compiled_query() else {
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
        // The sort is built OVER the rows the filters kept, so any change to
        // the filters invalidates it. Rebuilding here — one call site, right
        // where the filter changes — is what keeps the two composed rather
        // than racing.
        self.rebuild_sort_order();
    }

    /// The rows a sort is allowed to choose from: whatever the filters left.
    ///
    /// This is the composition contract. Sorting the whole sheet and then
    /// filtering would show hidden records; filtering and then sorting the
    /// survivors is the only order that keeps both true at once.
    fn sort_candidates(&self) -> Vec<u32> {
        if let Some(f) = &self.row_filter {
            return f.rows().to_vec();
        }
        let rows = self.wb.view().row_count();
        match (self.tables.first(), &self.table_mask) {
            (Some(_), Some(m)) => (0..m.visible_rows())
                .filter_map(|v| m.nth_visible(v).map(|d| d as u32))
                .collect(),
            _ => (0..rows as u32).collect(),
        }
    }

    /// Rebuild the sort mapping from the current spec and candidate rows.
    ///
    /// Called when a header is clicked and when the filters change — never
    /// from the paint path, which only borrows the finished mapping.
    fn rebuild_sort_order(&mut self) {
        if self.sort_keys.is_empty() {
            self.sort_order = None;
            // The subtotal plan is built OVER the rows the sort resolves, so
            // clearing a sort invalidates it just as changing one does. ONE
            // call site, right where the mapping underneath it changes.
            self.rebuild_subtotals();
            return;
        }
        let candidates = self.sort_candidates();
        let view = self.wb.view();
        self.sort_order = Some(ferrix_core::SortOrder::build(
            &candidates,
            &self.sort_keys,
            &view,
        ));
        self.rebuild_subtotals();
    }

    /// A column header was clicked: cycle its sort asc -> desc -> none.
    ///
    /// `additive` (shift-click) makes the column a secondary key instead of
    /// replacing the spec, which is the "sort by this, then by that" case.
    ///
    /// Sorting is a VIEW TRANSFORM: nothing is written, so this must not
    /// dirty the workbook and must not push an undo entry. The user undoes a
    /// sort by clicking the header again.
    pub fn sort_by_column(&mut self, col: usize, additive: bool) {
        // Issue #42: sorting is one of the granular allowances. Sorting a
        // protected sheet reorders every row under the author's locked cells,
        // so it is gated even though it moves no data of its own.
        if let Some(d) = self
            .wb
            .protection()
            .deny_action(ferrix_core::ProtectAction::Sort)
        {
            self.status = format!("Sort refused — {d}");
            return;
        }
        // An open edit belongs to a row whose screen position is about to
        // change underneath it; commit rather than let it land elsewhere.
        if self.editing.is_some() {
            self.commit_edit();
        }
        ferrix_core::cycle_click(&mut self.sort_keys, col as u32, additive);
        self.rebuild_sort_order();
        self.status = match self.sort_order.as_ref().and_then(|o| o.dir_of(col as u32)) {
            Some(ferrix_core::SortDir::Asc) => format!(
                "Sorted by {} ascending — a view only; no data moved",
                ferrix_core::column_name(col as u32)
            ),
            Some(ferrix_core::SortDir::Desc) => format!(
                "Sorted by {} descending — a view only; no data moved",
                ferrix_core::column_name(col as u32)
            ),
            None => "Sort cleared — original order restored".to_string(),
        };
        // The cursor keeps its underlying row, so it follows its record to
        // wherever the sort put it rather than staying at a screen position.
        self.scroll_to_selection();
    }

    // ========================================================================
    // Issue #34: Remove Duplicates, Subtotals, Consolidate
    // ========================================================================

    /// Cap on rows one Remove Duplicates can drop in a single undo step.
    ///
    /// The removed positions are the ONE per-removed-row allocation in the
    /// whole feature (4 bytes each), and this bounds it. Ten million is 40 MB
    /// — well inside the budget, far past anything a user reaches by hand,
    /// and small enough that a mis-keyed dedupe on a 200M-row sheet is refused
    /// with a sentence instead of an out-of-memory kill.
    pub const MAX_DEDUPE_REMOVED: usize = 10_000_000;

    /// Remove duplicate rows keyed on the SELECTED COLUMNS.
    ///
    /// The selection is the key chooser: selecting B:D and running this
    /// dedupes on those three columns, matching how the rest of this app
    /// scopes an action. A single-cell selection means "the whole row",
    /// which is Excel's default when every checkbox is ticked.
    pub fn remove_duplicates(&mut self) {
        if self.editing.is_some() {
            self.commit_edit();
        }
        let (tl, br) = self.selection.bounds();
        // A one-column-wide selection is still a deliberate key choice; only a
        // selection spanning ONE cell falls back to the whole row.
        let key_cols: Vec<u32> = if self.selection.cell_count() <= 1 {
            Vec::new()
        } else {
            (tl.col..=br.col).collect()
        };
        let label = if key_cols.is_empty() {
            "every column".to_string()
        } else if key_cols.len() == 1 {
            format!("column {}", ferrix_core::column_name(key_cols[0]))
        } else {
            format!(
                "columns {}:{}",
                ferrix_core::column_name(tl.col),
                ferrix_core::column_name(br.col)
            )
        };
        match self
            .wb
            .remove_duplicates(&key_cols, Self::MAX_DEDUPE_REMOVED)
        {
            Ok(rep) if rep.duplicates == 0 => {
                self.status = format!("No duplicate rows found on {label}");
            }
            Ok(rep) => {
                // Every view transform was built over the OLD row count, so
                // they are stale the moment rows go. Rebuilt through the same
                // one call site the filters use, so nothing can resolve
                // through a mapping that outlived its rows.
                self.rebuild_row_filter();
                self.rebuild_subtotals();
                self.status = format!(
                    "Removed {} duplicate row{} on {label} — {} unique row{} kept, \
                     one Ctrl+Z restores them all",
                    rep.duplicates,
                    if rep.duplicates == 1 { "" } else { "s" },
                    rep.unique,
                    if rep.unique == 1 { "" } else { "s" },
                );
            }
            Err(e) => self.status = e,
        }
        self.sync_formula_bar();
    }

    /// Group by the CURSOR's column and show a subtotal at each change of
    /// value; running it again removes the subtotals.
    ///
    /// A VIEW TRANSFORM: nothing is written, nothing is dirtied, and no undo
    /// entry is pushed. The user removes subtotals by running the command
    /// again, exactly as they clear a sort by clicking the header again.
    pub fn toggle_subtotals(&mut self) {
        if self.subtotals.is_some() {
            self.subtotals = None;
            self.subtotal_spec = None;
            self.status =
                "Subtotals removed — the original view is back, exactly as it was".to_string();
            return;
        }
        if self.editing.is_some() {
            self.commit_edit();
        }
        let group_col = self.selection.cursor.col;
        // Every OTHER column in the selection is aggregated; a single-cell
        // selection aggregates the column to the right of the group column,
        // which is the "group by A, total B" shape almost every subtotal has.
        let (tl, br) = self.selection.bounds();
        let agg_cols: Vec<u32> = if self.selection.cell_count() <= 1 {
            vec![group_col + 1]
        } else {
            (tl.col..=br.col).filter(|&c| c != group_col).collect()
        };
        self.subtotal_spec = Some((group_col, agg_cols, ferrix_core::SubtotalFn::Sum));
        self.rebuild_subtotals();
        match &self.subtotals {
            Some(p) => {
                self.status = format!(
                    "Subtotalled by {} — {} group{}; a view only, no rows inserted",
                    ferrix_core::column_name(group_col),
                    p.groups().len(),
                    if p.groups().len() == 1 { "" } else { "s" }
                )
            }
            // `rebuild_subtotals` already put the refusal in the status line.
            None => self.subtotal_spec = None,
        }
    }

    /// Rebuild the subtotal plan over whatever the other transforms resolve.
    ///
    /// Called when the spec changes and when the rows underneath it change —
    /// never from the paint path, which only borrows the finished plan. This
    /// is the same discipline `rebuild_sort_order` follows, and it is what
    /// makes subtotals COMPOSE with sort and filter rather than race them.
    fn rebuild_subtotals(&mut self) {
        let Some((group_col, agg_cols, func)) = self.subtotal_spec.clone() else {
            self.subtotals = None;
            return;
        };
        // The rows the stages BELOW this one resolve. Asking the resolver
        // itself would be circular — it consults this plan — so the count is
        // taken from the same mapping the resolver would, with the subtotal
        // stage absent.
        let below = crate::grid::RowResolver {
            filter: self.row_filter.as_ref(),
            sort: self.sort_order.as_ref(),
            table: None,
            pad: None,
            hidden: self.hidden_rows.as_ref(),
            subtotals: None,
        };
        let data_rows = self.wb.view().row_count();
        let rows = below.resolved_rows(data_rows);
        let view = self.wb.view();
        let src = SubtotalRows {
            view: &view,
            below: &below,
            group_col,
        };
        match ferrix_core::SubtotalPlan::build(
            rows,
            group_col,
            agg_cols,
            func,
            &src,
            ferrix_core::MAX_GROUPS,
        ) {
            Ok(p) => self.subtotals = Some(p),
            Err(e) => {
                self.subtotals = None;
                self.status = format!("Subtotals refused — {e}");
            }
        }
    }

    /// The subtotal plan the grid renders through, for tests.
    pub fn subtotal_plan(&self) -> Option<&ferrix_core::SubtotalPlan> {
        self.subtotals.as_ref()
    }

    /// SUBTOTAL rows and aggregate texts the last frame painted (issue #34).
    ///
    /// Real paint output, not model state: a plan that exists but never
    /// reaches the screen reports zero here, which is the failure mode
    /// "model-complete and unreachable" actually looks like.
    pub fn last_subtotal_rows(&self) -> usize {
        self.last_subtotal_rows
    }

    pub fn last_subtotal_texts(&self) -> usize {
        self.last_subtotal_texts
    }

    /// The first `n` screen rows, resolved through THE resolver.
    ///
    /// Exposed for tests so an assertion about row identity goes through the
    /// same code the painter does, rather than through a mapping the test
    /// built for itself — which would agree right up until the moment the
    /// real one was wrong.
    pub fn screen_rows(&self, n: usize) -> Vec<crate::grid::ScreenRow> {
        let r = self.row_resolver(None);
        (0..n).filter_map(|i| r.resolve(i)).collect()
    }

    /// Consolidate the same range from every sheet in the workbook into a
    /// labelled block starting at the cursor.
    ///
    /// The SELECTION names the source rectangle — its first row is the column
    /// headers and its first column is the row keys — and every sheet is read
    /// at those coordinates. Keys missing from a sheet are reported, never
    /// zeroed; see `consolidate.rs`.
    pub fn consolidate_sheets(&mut self) {
        if self.editing.is_some() {
            self.commit_edit();
        }
        if self.wb.sheet_count() < 2 {
            self.status =
                "Consolidate needs at least two sheets — this workbook has one".to_string();
            return;
        }
        let (tl, br) = self.selection.bounds();
        let sources: Vec<ferrix_core::consolidate::Source> = (0..self.wb.sheet_count())
            .filter_map(|i| self.wb.sheet_name_at(i))
            .map(|name| ferrix_core::consolidate::Source {
                sheet: name,
                first_row: tl.row,
                last_row: br.row,
                first_col: tl.col,
                last_col: br.col,
            })
            .collect();
        let req = ferrix_core::ConsolidateRequest {
            sources,
            func: ferrix_core::SubtotalFn::Sum,
            max_cells: ferrix_core::MAX_OUTPUT_CELLS,
        };
        let src = WorkbookRanges { wb: &self.wb };
        let out = match ferrix_core::consolidate(&req, &src) {
            Ok(o) => o,
            Err(e) => {
                self.status = format!("Consolidate refused — {e}");
                return;
            }
        };
        // Written below the source block, so the source the user selected is
        // never overwritten by its own consolidation.
        let dest_row = br.row + 2;
        let mut writes: Vec<(ferrix_core::CellRef, String)> = Vec::new();
        for (ci, ck) in out.col_keys.iter().enumerate() {
            writes.push((
                ferrix_core::CellRef::new(dest_row, tl.col + 1 + ci as u32),
                ck.clone(),
            ));
        }
        for (ri, rk) in out.row_keys.iter().enumerate() {
            let r = dest_row + 1 + ri as u32;
            writes.push((ferrix_core::CellRef::new(r, tl.col), rk.clone()));
            for ci in 0..out.col_keys.len() {
                let c = tl.col + 1 + ci as u32;
                // A cell no source contributed to is left EMPTY rather than
                // written as 0 — the whole point of the feature is that the
                // reader can tell the two apart on the sheet, not just in the
                // status line.
                if let Some(v) = out.at(ri, ci).and_then(|c| c.value) {
                    writes.push((
                        ferrix_core::CellRef::new(r, c),
                        ferrix_core::format_number(v),
                    ));
                }
            }
        }
        let n = writes.len();
        if let Err(e) = self.wb.write_cells_bulk(writes) {
            self.status = format!("Consolidate refused — {e}");
            return;
        }
        self.rebuild_row_filter();
        self.rebuild_subtotals();
        let partial = out.report.partial_cells;
        self.status = if partial > 0 {
            format!(
                "Consolidated {} sheets into {n} cells — {partial} cell{} came from \
                 fewer than all of them; {} key/sheet pair{} missing (not zeroed)",
                out.report.sources,
                if partial == 1 { "" } else { "s" },
                out.report.missing.len(),
                if out.report.missing.len() == 1 {
                    ""
                } else {
                    "s"
                },
            )
        } else {
            format!(
                "Consolidated {} sheets into {n} cells — every key present on every sheet",
                out.report.sources
            )
        };
        self.sync_formula_bar();
    }

    /// The sort mapping the grid should render through, if any.
    pub fn sort_order(&self) -> Option<&ferrix_core::SortOrder> {
        self.sort_order.as_ref()
    }

    /// Scroll the BODY pane to a given screen row. Clamped by the grid on the
    /// next frame exactly as a wheel or scrollbar drag would be.
    pub fn scroll_body_to(&mut self, screen_row: f64) {
        self.scroll.row_offset = screen_row.max(0.0);
    }

    /// Where the body pane is scrolled to, in screen rows.
    pub fn body_row_offset(&self) -> f64 {
        self.scroll.row_offset
    }

    /// (screen row, underlying row) for every row the last frame painted,
    /// frozen band FIRST. The app's own account of what is on screen.
    pub fn painted_rows(&self) -> &[(usize, u32)] {
        &self.last_painted_rows
    }

    /// How many painted rows belong to the frozen / split band.
    pub fn frozen_row_count(&self) -> usize {
        self.last_frozen_rows
    }

    /// Underlying rows that were on screen last frame, in paint order.
    pub fn painted_underlying_rows(&self) -> Vec<u32> {
        self.last_painted_rows.iter().map(|&(_, r)| r).collect()
    }

    /// Centre of a CELL as it would be painted right now, or `None` when it
    /// is off screen.
    ///
    /// Read back from the app's own geometry — the same `cell_screen_rect` the
    /// in-cell editor is positioned with, at the current zoom and pane
    /// configuration — rather than computed from constants in the test. The
    /// grid moves whenever a bar opens above it or the zoom changes, so a test
    /// that hard-codes pixels ends up clicking somewhere else and reporting a
    /// working feature as broken.
    pub fn cell_center(&self, cell: CellRef) -> Option<(f32, f32)> {
        let r = self.cell_rect(cell)?;
        let c = r.center();
        Some((c.x, c.y))
    }

    /// Screen rect of a cell as it would be painted right now.
    ///
    /// Goes through `cell_screen_rect_h` with the app's OWN `RowHeights`, so
    /// a wrapped row's rect is as tall here as it is on screen (issue #28).
    /// Without that, the in-cell editor and every test reading geometry back
    /// would use a 22px row where the grid painted a 44px one.
    pub fn cell_rect(&self, cell: CellRef) -> Option<egui::Rect> {
        let outer = self.last_grid_rect?;
        let view = self.wb.view();
        let heights = crate::grid::RowHeights::new(Some(&self.wb.format), &view, &self.col_widths);
        Grid::cell_screen_rect_h(
            cell,
            outer,
            &self.scroll,
            &self.col_widths,
            &self.row_resolver(self.pad_space()),
            crate::grid::Metrics::new(self.zoom),
            self.panes,
            Some(&heights),
        )
    }

    /// Which cell a viewport point is over (issue #38).
    ///
    /// The inverse of [`Self::cell_center`], and deliberately built the same
    /// way — by asking `cell_screen_rect` where each cell IS rather than by
    /// inverting the layout arithmetic by hand. A second, independent
    /// coordinate mapping is precisely what the guide warns about: it would
    /// agree with the paint loop until a freeze, a filter or a sort was
    /// active, and then quietly point at the wrong row.
    ///
    /// Bounded by the viewport: it searches only the rows the last frame
    /// actually painted, so this is a viewport-sized scan on any sheet.
    fn cell_at_point(&self, p: egui::Pos2, outer: egui::Rect) -> Option<CellRef> {
        let resolver = self.row_resolver(self.pad_space());
        let metrics = crate::grid::Metrics::new(self.zoom);
        let cols = self.col_widths.len().max(self.navigable_cols());
        // Through the SAME heights the grid painted with, so a wrapped row is
        // as tall here as it looks (issue #28).
        let view = self.wb.view();
        let heights = crate::grid::RowHeights::new(Some(&self.wb.format), &view, &self.col_widths);
        for (_, row) in &self.last_painted_rows {
            for c in 0..cols {
                let cell = CellRef::new(*row, c as u32);
                if let Some(r) = Grid::cell_screen_rect_h(
                    cell,
                    outer,
                    &self.scroll,
                    &self.col_widths,
                    &resolver,
                    metrics,
                    self.panes,
                    Some(&heights),
                ) {
                    if r.contains(p) {
                        return Some(cell);
                    }
                }
            }
        }
        None
    }

    /// Centre of a display column's header, as painted last frame.
    pub fn header_center(&self, col: usize) -> Option<(f32, f32)> {
        self.header_hitboxes
            .iter()
            .find(|(c, _)| *c == col)
            .map(|(_, p)| (p.x, p.y))
    }

    /// Direction currently applied to a display column, for tests and the
    /// header indicator.
    pub fn sort_dir(&self, col: usize) -> Option<ferrix_core::SortDir> {
        self.sort_order.as_ref().and_then(|o| o.dir_of(col as u32))
    }

    /// The underlying row shown at a given screen row — the single question a
    /// test about ordering actually wants answered.
    pub fn visible_row_order(&self) -> Vec<u32> {
        match (&self.sort_order, &self.row_filter) {
            (Some(s), _) => s.rows().to_vec(),
            (None, Some(f)) => f.rows().to_vec(),
            (None, None) => (0..self.wb.view().row_count() as u32).collect(),
        }
    }

    /// The row resolution the grid builds, for callers outside `show()` (the
    /// cell editor) and for tests. ONE construction site, so a caller cannot
    /// accidentally resolve through a subset of the active transforms.
    fn row_resolver<'a>(
        &'a self,
        pad: Option<crate::grid::PadSpace>,
    ) -> crate::grid::RowResolver<'a> {
        crate::grid::RowResolver {
            filter: self.row_filter.as_ref(),
            sort: self.sort_order.as_ref(),
            subtotals: self.subtotals.as_ref(),
            // The table's own decoration is rebuilt per frame inside the
            // paint closure; outside it the mask is only needed when neither
            // of the other two transforms is active, and in that case the
            // grid's own resolver handles it.
            table: None,
            pad,
            hidden: self.hidden_rows.as_ref(),
        }
    }

    // --- row and column sizing, hiding, grouping (issue #29) ---
    //
    // Every operation below is a PLAIN METHOD with no gesture attached, and
    // the harness tests drive these directly. The pointer gestures in the
    // paint loop are thin wrappers that call them, so a test proves the
    // operation preserves meaning rather than proving drag arithmetic.

    /// How many rows autofit is allowed to inspect beyond the viewport.
    ///
    /// The whole point of the bound: autofitting a column of a 200M-row sheet
    /// must return instantly, so it measures what is on screen plus a capped
    /// sample and NEVER the column. A wider sample buys a slightly better
    /// width and costs unbounded time on exactly the files Ferrix exists for.
    pub const AUTOFIT_SAMPLE: usize = 1000;

    /// Width column `col` should take to fit its visible content.
    ///
    /// Returns the width AND the number of rows actually inspected, so the
    /// bound can be asserted rather than inferred from a suspiciously fast
    /// test. The count is the honest one: every row whose display string was
    /// materialised.
    pub fn autofit_width_measured(&self, col: usize) -> (f32, usize) {
        let view = self.wb.view();
        let rows = view.row_count();
        let mut inspected = 0usize;
        let mut widest = view.header_or_letter(col).len();

        // 1. The rows actually on screen — what the user is looking at, and
        //    the reason a visible long value is never clipped by autofit.
        let mut seen: Vec<u32> = self.last_painted_rows.iter().map(|&(_, r)| r).collect();

        // 2. A bounded sample STRIDED over the sheet, so the width reflects
        //    the whole column's shape rather than just its first screenful.
        //    The stride is what keeps this O(SAMPLE) instead of O(rows).
        let budget = Self::AUTOFIT_SAMPLE;
        let step = (rows / budget).max(1);
        let mut r = 0usize;
        while r < rows && seen.len() < budget + self.last_painted_rows.len() {
            seen.push(r as u32);
            r += step;
        }
        seen.sort_unstable();
        seen.dedup();

        for row in seen {
            if (row as usize) >= rows && rows > 0 {
                continue;
            }
            let text = view.display(CellRef::new(row, col as u32));
            widest = widest.max(text.chars().count());
            inspected += 1;
        }
        (width_for(widest), inspected)
    }

    /// Autofit a column to its visible content, bounded by [`Self::AUTOFIT_SAMPLE`].
    pub fn autofit_column(&mut self, col: usize) {
        let (w, inspected) = self.autofit_width_measured(col);
        self.last_autofit_rows = inspected;
        self.set_col_width(col, w);
        self.status = format!(
            "Autofit column {} to {:.0}px from {} sampled row{}",
            ferrix_core::column_name(col as u32),
            w,
            inspected,
            if inspected == 1 { "" } else { "s" }
        );
    }

    /// Rows the last autofit inspected, for tests that assert the BOUND.
    pub fn last_autofit_rows(&self) -> usize {
        self.last_autofit_rows
    }

    /// Outline toggle buttons the gutter painted last frame.
    pub fn outline_button_count(&self) -> usize {
        self.last_outline_buttons
    }

    /// The header hide/unhide menu, when open. `(column, position)`.
    pub fn header_menu_col(&self) -> Option<usize> {
        self.header_menu.map(|(c, _)| c)
    }

    /// Open the header menu without a pointer, for the harness.
    pub fn open_header_menu(&mut self, col: usize) {
        self.header_menu = Some((col, egui::pos2(0.0, 0.0)));
    }

    /// The hide/unhide menu a right-click on a header opens.
    ///
    /// Painted as an egui Area so it floats above the grid; the grid already
    /// suppresses its own hit-testing while the pointer is over a floating
    /// area, so clicking a menu item cannot also land on the cell beneath.
    fn show_header_menu(&mut self, ctx: &egui::Context) {
        let Some((col, pos)) = self.header_menu else {
            return;
        };
        let mut close = false;
        egui::Area::new(egui::Id::new("ferrix_header_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(180.0);
                    ui.label(
                        RichText::new(format!("Column {}", ferrix_core::column_name(col as u32)))
                            .strong(),
                    );
                    ui.separator();
                    if ui.button("Hide").clicked() {
                        self.set_col_hidden(col, true);
                        close = true;
                    }
                    // Unhiding names the NEIGHBOURS, because a hidden column
                    // cannot be right-clicked: it has no pixels. This is the
                    // only way back for the column the user just hid.
                    let hidden_left = (0..col).rev().find(|&c| self.is_col_hidden(c));
                    let hidden_right = (col + 1..self.stats_cols).find(|&c| self.is_col_hidden(c));
                    if let Some(h) = hidden_left {
                        if ui
                            .button(format!(
                                "Unhide {} (left)",
                                ferrix_core::column_name(h as u32)
                            ))
                            .clicked()
                        {
                            self.set_col_hidden(h, false);
                            close = true;
                        }
                    }
                    if let Some(h) = hidden_right {
                        if ui
                            .button(format!(
                                "Unhide {} (right)",
                                ferrix_core::column_name(h as u32)
                            ))
                            .clicked()
                        {
                            self.set_col_hidden(h, false);
                            close = true;
                        }
                    }
                    if ui.button("Unhide all columns").clicked() {
                        self.unhide_all_cols();
                        close = true;
                    }
                    ui.separator();
                    if ui.button("Autofit width").clicked() {
                        self.autofit_column(col);
                        close = true;
                    }
                    if ui.button("Reset width").clicked() {
                        self.set_col_width(col, crate::grid::DEFAULT_COL_WIDTH);
                        close = true;
                    }
                    ui.separator();
                    // Row operations on the current selection, so grouping and
                    // hiding rows are reachable without a second menu.
                    let (r0, r1) = self.selection.row_range();
                    if ui
                        .button(format!("Hide rows {}–{}", r0 + 1, r1 + 1))
                        .clicked()
                    {
                        self.hide_rows(r0, r1);
                        close = true;
                    }
                    if ui.button("Unhide all rows").clicked() {
                        let n = self.wb.view().row_count() as u32;
                        self.unhide_rows(0, n.saturating_sub(1));
                        close = true;
                    }
                    if ui
                        .button(format!("Group rows {}–{}", r0 + 1, r1 + 1))
                        .clicked()
                    {
                        if let Err(e) = self.group_rows(r0, r1) {
                            self.status = format!("Group failed: {e}");
                        }
                        close = true;
                    }
                    if ui.button("Ungroup rows").clicked() {
                        if !self.ungroup_rows(r0) {
                            self.status = "No group at that row".to_string();
                        }
                        close = true;
                    }
                    ui.separator();
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.header_menu = None;
        }
    }

    /// Set a column's width, keeping the dense paint vector and the persisted
    /// sizing map in step. Both are updated HERE, in one place, so a width can
    /// never paint at one size and save at another.
    pub fn set_col_width(&mut self, col: usize, w: f32) {
        let w = w.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);
        if self.col_widths.len() <= col {
            self.col_widths
                .resize(col + 1, crate::grid::DEFAULT_COL_WIDTH);
        }
        self.col_widths[col] = w;
        self.sizing.cols.set_width(col as u32, w);
        self.mark_sizing_dirty();
    }

    /// Width a column currently paints at.
    pub fn col_width(&self, col: usize) -> f32 {
        self.col_widths
            .get(col)
            .copied()
            .unwrap_or(crate::grid::DEFAULT_COL_WIDTH)
    }

    /// Hide or unhide a column.
    pub fn set_col_hidden(&mut self, col: usize, hidden: bool) {
        if hidden {
            self.sizing.cols.hide(col as u32);
        } else {
            self.sizing.cols.unhide(col as u32);
        }
        self.hidden_cols = self.sizing.hidden_col_set();
        self.mark_sizing_dirty();
        self.status = format!(
            "{} column {}",
            if hidden { "Hid" } else { "Unhid" },
            ferrix_core::column_name(col as u32)
        );
    }

    pub fn toggle_col_hidden(&mut self, col: usize) {
        self.set_col_hidden(col, !self.is_col_hidden(col));
    }

    pub fn is_col_hidden(&self, col: usize) -> bool {
        self.hidden_cols.contains(&(col as u32))
    }

    /// Unhide every column — the escape hatch for a column the user cannot
    /// select because it is not on screen.
    pub fn unhide_all_cols(&mut self) {
        let n = self.sizing.cols.hidden_count();
        self.sizing.cols.unhide_all();
        self.hidden_cols = self.sizing.hidden_col_set();
        self.mark_sizing_dirty();
        self.status = format!("Unhid {n} column{}", if n == 1 { "" } else { "s" });
    }

    /// Set an explicit row height. Zero hides the row, matching Excel.
    pub fn set_row_height(&mut self, first: u32, last: u32, h: f32) {
        self.sizing.rows.set_range(first, last, h);
        self.rebuild_hidden_rows();
    }

    /// Hide rows by setting their height to zero.
    pub fn hide_rows(&mut self, first: u32, last: u32) {
        self.sizing.rows.hide(first, last);
        self.rebuild_hidden_rows();
        self.status = format!("Hid rows {}–{}", first + 1, last + 1);
    }

    pub fn unhide_rows(&mut self, first: u32, last: u32) {
        self.sizing.rows.unhide(first, last);
        self.rebuild_hidden_rows();
        self.status = format!("Unhid rows {}–{}", first + 1, last + 1);
    }

    pub fn is_row_hidden(&self, row: u32) -> bool {
        self.hidden_rows.as_ref().is_some_and(|h| h.is_hidden(row))
    }

    /// Group rows into an outline level.
    pub fn group_rows(
        &mut self,
        first: u32,
        last: u32,
    ) -> Result<u8, ferrix_core::sizing::OutlineError> {
        let level = self.sizing.row_outline.group(first, last)?;
        self.rebuild_hidden_rows();
        self.status = format!("Grouped rows {}–{} at level {level}", first + 1, last + 1);
        Ok(level)
    }

    /// Remove the innermost group covering a row.
    pub fn ungroup_rows(&mut self, row: u32) -> bool {
        let removed = self.sizing.row_outline.ungroup_at(row).is_some();
        if removed {
            self.rebuild_hidden_rows();
            self.status = format!("Ungrouped at row {}", row + 1);
        }
        removed
    }

    /// Collapse or expand the group starting at `row`.
    pub fn toggle_row_group(&mut self, row: u32) -> Option<bool> {
        let collapsed = self.sizing.row_outline.toggle_at(row)?;
        self.rebuild_hidden_rows();
        self.status = format!(
            "{} group at row {}",
            if collapsed { "Collapsed" } else { "Expanded" },
            row + 1
        );
        Some(collapsed)
    }

    /// Outline nesting level covering a row, 0 when ungrouped.
    pub fn row_outline_level(&self, row: u32) -> u8 {
        self.sizing.row_outline.level_at(row)
    }

    /// Show only outline levels up to `level`.
    pub fn collapse_rows_to_level(&mut self, level: u8) {
        self.sizing.row_outline.collapse_to_level(level);
        self.rebuild_hidden_rows();
        self.status = format!("Showing outline levels 1–{level}");
    }

    /// The sizing state, for persistence and tests.
    pub fn sizing(&self) -> &ferrix_core::sizing::SheetSizing {
        &self.sizing
    }

    /// Replace the sizing state wholesale — the load path.
    pub fn set_sizing(&mut self, s: ferrix_core::sizing::SheetSizing) {
        self.sizing = s;
        // The dense width vector is the paint path's copy of the same facts,
        // so it is rebuilt from the loaded map rather than left stale.
        for (c, w) in self.sizing.cols.widths() {
            let c = c as usize;
            if self.col_widths.len() <= c {
                self.col_widths
                    .resize(c + 1, crate::grid::DEFAULT_COL_WIDTH);
            }
            self.col_widths[c] = w;
        }
        self.hidden_cols = self.sizing.hidden_col_set();
        self.rebuild_hidden_rows();
    }

    /// Rebuild the folded hidden-row index.
    ///
    /// ONE index, rebuilt whenever anything that hides a row changes, and
    /// handed to the resolver as a single stage. Cost is O(spans).
    fn rebuild_hidden_rows(&mut self) {
        let h = self.sizing.hidden_rows();
        self.hidden_rows = (!h.is_empty()).then_some(h);
        self.mark_sizing_dirty();
    }

    fn mark_sizing_dirty(&mut self) {
        self.sizing_dirty = true;
    }

    /// Turn filter mode on or off, keeping the viewport and selection anchored
    /// to the row the user was looking at.
    pub fn toggle_filter_mode(&mut self) {
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
        // Through the SAME resolver the grid paints with — a sorted view puts
        // a row somewhere the filter alone would not, and scrolling to a stale
        // position is how "the cursor is off screen after a sort" happens.
        self.row_resolver(self.pad_space())
            .visible_of(row)
            .map(|v| v as f64)
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
        self.replace_open = false;
        self.search_results = ferrix_core::SearchResults::default();
        self.search_index = 0;
        // Closing search must restore the full sheet: leaving rows hidden with
        // no visible search bar would look like data loss.
        self.row_filter = None;
        // Re-anchor the viewport in unfiltered row space.
        self.scroll_to_selection();
        self.focus = Focus::Grid;
    }

    // ------------------------------------------------------------- replace

    /// Compile the current search box into a [`ferrix_core::Query`].
    ///
    /// One place, so Ctrl+F and Ctrl+H can never disagree about what a match
    /// is. A malformed regex is recorded in `search_regex_error` and returns
    /// `None` — the caller then finds nothing, and the panel says why.
    fn compiled_query(&mut self) -> Option<ferrix_core::Query> {
        let raw = self.search_input.trim().to_string();
        match ferrix_core::Query::compile(
            &raw,
            self.search_case_sensitive,
            self.search_whole_cell,
            self.search_regex,
        ) {
            Ok(q) => {
                self.search_regex_error = None;
                q
            }
            Err(e) => {
                self.search_regex_error = Some(e);
                None
            }
        }
    }

    fn replace_spec(&mut self) -> Option<ferrix_core::ReplaceSpec> {
        let query = self.compiled_query()?;
        Some(ferrix_core::ReplaceSpec::new(
            query,
            self.replace_input.clone(),
            self.replace_look_in,
        ))
    }

    /// Replace at the current match, then advance to the next one.
    fn replace_current(&mut self) {
        let Some(spec) = self.replace_spec() else {
            self.status = match &self.search_regex_error {
                Some(e) => format!("Bad pattern: {e}"),
                None => "Nothing to replace — type something to find".into(),
            };
            return;
        };
        let Some(cell) = self.search_results.wrapped(self.search_index) else {
            self.status = "No match to replace".into();
            return;
        };
        match self.wb.replace_one(cell, &spec) {
            Some(new_text) => {
                self.status = format!(
                    "Replaced in {}{} → {new_text:?}",
                    ferrix_core::column_name(cell.col),
                    cell.row + 1
                );
                // The sheet changed under the result list, so re-find before
                // advancing — otherwise "next" could step onto a stale hit.
                self.run_search();
                self.next_match();
            }
            None => {
                self.status = "That cell no longer matches".into();
                self.next_match();
            }
        }
    }

    /// Replace everywhere, as ONE undo step.
    ///
    /// Runs on the UI thread deliberately: the pass polls `replace_cancel` and
    /// reports progress, and moving it off-thread would require snapshotting
    /// the overlay and merging edits back, which is a much larger change than
    /// this feature needs. The window loop keeps it responsive to cancel.
    fn replace_all(&mut self) {
        let Some(spec) = self.replace_spec() else {
            self.status = match &self.search_regex_error {
                Some(e) => format!("Bad pattern: {e}"),
                None => "Nothing to replace — type something to find".into(),
            };
            return;
        };
        // Derived from live memory, not a magic number: every replaced cell
        // costs an overlay entry plus a before/after pair in the single undo
        // entry, and a term matching 80M cells on a 200M-row sheet would
        // otherwise build an undo entry larger than RAM.
        let max_edits = ferrix_core::Budget::sample()
            .max_units_usize(ferrix_core::budget::cost::REPLACE_CELL)
            .min(20_000_000);

        self.replace_cancel.reset();
        let cancel = self.replace_cancel.clone();
        // Deterministic cancellation for tests: cancel from inside the pass's
        // own progress callback at a known point, rather than racing a timer
        // against it from another thread. A flaky cancel test proves nothing
        // about cancel, and a spinning helper thread slows every other test
        // sharing the machine.
        let trip = self.replace_cancel_after_applied;
        let cancel_for_cb = cancel.clone();
        let report = self
            .wb
            .replace_all(&spec, max_edits, &cancel, |_examined, applied| {
                if let Some(n) = trip {
                    if applied >= n {
                        cancel_for_cb.cancel();
                    }
                }
            });
        self.replace_progress = None;

        self.status = report.describe();
        // The result list is stale the moment cells change; re-find so the
        // count and the highlights describe the sheet as it is now.
        self.run_search();
    }

    /// Whether the replace panel is open.
    pub fn replace_is_open(&self) -> bool {
        self.replace_open
    }

    /// The replace panel's replacement text.
    #[allow(dead_code)] // harness API
    pub fn replace_text_input(&self) -> &str {
        &self.replace_input
    }

    /// Which text a replace reads.
    #[allow(dead_code)] // harness API
    pub fn replace_look_in(&self) -> ferrix_core::LookIn {
        self.replace_look_in
    }

    /// Set the replacement text. The panel's TextEdit writes the same field;
    /// this is how a test drives it without pixel-hunting the box.
    pub fn set_replace_input(&mut self, s: &str) {
        self.replace_input = s.to_string();
    }

    pub fn set_search_input(&mut self, s: &str) {
        self.search_input = s.to_string();
        self.run_search();
    }

    pub fn set_replace_look_in(&mut self, look_in: ferrix_core::LookIn) {
        self.replace_look_in = look_in;
    }

    pub fn set_search_regex(&mut self, on: bool) {
        self.search_regex = on;
        self.run_search();
    }

    pub fn set_search_case_sensitive(&mut self, on: bool) {
        self.search_case_sensitive = on;
        self.run_search();
    }

    pub fn set_search_whole_cell(&mut self, on: bool) {
        self.search_whole_cell = on;
        self.run_search();
    }

    /// A handle to the running replace's cancel flag, so a test (or the
    /// Cancel button) can stop a pass in flight.
    pub fn replace_cancel_token(&self) -> ferrix_core::CancelToken {
        self.replace_cancel.clone()
    }

    /// Arrange for the next Replace All to cancel itself once `n` cells have
    /// been applied. Test seam; see the field's docs.
    pub fn cancel_replace_after(&mut self, n: usize) {
        self.replace_cancel_after_applied = Some(n);
    }

    /// Shrink the Replace All scan window so a small fixture still crosses
    /// several window boundaries. Test seam.
    pub fn set_replace_window_rows(&mut self, rows: usize) {
        self.wb.set_replace_window_rows(rows);
    }

    /// Run Replace All directly, bypassing the button.
    ///
    /// The panel's button calls exactly this. Exposed because the alternative
    /// — synthesising a click on a button whose pixel position depends on the
    /// theme's text metrics — would test layout arithmetic rather than
    /// replace behaviour.
    pub fn do_replace_all(&mut self) {
        self.replace_all();
    }

    /// Run a single Replace at the current match. Same rationale as
    /// [`FerrixApp::do_replace_all`].
    pub fn do_replace_one(&mut self) {
        self.replace_current();
    }

    /// Centre the viewport on the selection — used when jumping to a match, so
    /// the hit lands mid-screen rather than scraping the edge.
    fn center_on_selection(&mut self) {
        let visible = (self.last_viewport_h / crate::grid::Metrics::new(self.zoom).row_h) as f64;
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
        self.formula_result = Some(match self.wb.parse_active(&text) {
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

        // --- issue #38: Ctrl+` and F4 ---
        //
        // Handled here, ahead of the `grid_has_keys` gate below, because both
        // have to work while a TextEdit owns the keyboard: F4 is meaningless
        // anywhere else, and Ctrl+` is a view toggle the user reaches for
        // mid-edit. `i.modifiers.command` is read from the AGGREGATE modifier
        // state, which is the only place egui reports it reliably.
        let (show_formulas_key, f4) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::Backtick),
                i.key_pressed(Key::F4),
            )
        });
        if show_formulas_key {
            self.toggle_show_formulas();
            return;
        }
        if f4 {
            self.cycle_reference_anchor();
            return;
        }

        let (ctrl_f, ctrl_h, ctrl_s, escape, f3, shift_f3) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::F),
                i.modifiers.command && i.key_pressed(Key::H),
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

        // --- zoom shortcuts (roadmap #6) ---
        //
        // Both the main-row and numpad keys, and both Plus and Equals: on a US
        // layout Ctrl+= is what "zoom in" physically is, and matching only
        // Plus makes the shortcut require Shift for no reason.
        let (zoom_in_key, zoom_out_key, zoom_reset_key) = ctx.input(|i| {
            let c = i.modifiers.command;
            (
                c && (i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals)),
                c && i.key_pressed(Key::Minus),
                c && i.key_pressed(Key::Num0),
            )
        });
        if zoom_in_key {
            self.zoom_in();
            return;
        }
        if zoom_out_key {
            self.zoom_out();
            return;
        }
        if zoom_reset_key {
            self.zoom_reset();
            return;
        }
        // --- trace precedents/dependents shortcuts (roadmap #39) ---
        let (trace_prec_key, trace_dep_key) = ctx.input(|i| {
            let c = i.modifiers.command;
            (
                c && i.key_pressed(Key::OpenBracket),
                c && i.key_pressed(Key::CloseBracket),
            )
        });
        if self.editing.is_none() && trace_prec_key {
            self.trace_precedents();
            return;
        }
        if self.editing.is_none() && trace_dep_key {
            self.trace_dependents();
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
        if ctrl_h {
            // Ctrl+H opens the replace panel BESIDE the search box, opening
            // search too if it was closed — replace without a find field to
            // type into would be a panel that cannot do anything.
            self.search_open = true;
            self.replace_open = true;
            self.focus = Focus::Search;
            // Focus the replacement box: the user pressing Ctrl+H already
            // knows what they are finding often enough that landing in "find"
            // would mean an extra Tab every time.
            self.replace_focus_pending = true;
            return;
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
                // Hand the keyboard back to the grid for real, not just in our
                // own bookkeeping. egui keeps focus on the (now hidden) text
                // box otherwise, and every subsequent grid chord — Ctrl+Z
                // included — is swallowed by a widget that is no longer
                // on screen.
                if let Some(id) = ctx.memory(|m| m.focused()) {
                    ctx.memory_mut(|m| m.surrender_focus(id));
                }
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
        // The comment editor owns the keyboard while it is open: arrow keys
        // must move the caret in the note, not the cursor around the grid.
        if widget_has_keyboard || self.focus != Focus::Grid || self.comment_editing.is_some() {
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

impl FerrixApp {
    // ---- the command registry (issue #40) ----
    //
    // These three are the whole app-side surface of the palette: a snapshot of
    // availability, one dispatcher, and the frame hook. The menu bar and the
    // palette both go through them, which is what makes "a command in a menu
    // but not in the palette" unrepresentable rather than merely unlikely.

    /// Snapshot of everything the registry needs to decide what can run now.
    ///
    /// A snapshot rather than a borrow: the menu-bar closure already holds
    /// `&mut self`, so it cannot also hold a reference into the app.
    pub fn command_state(&self) -> crate::command::CommandState {
        let busy = self.loading || self.exporting || self.compacting;
        crate::command::CommandState {
            can_compact: self.can_compact(),
            compact_hint: self.compact_hint(),
            can_save: self.wb.is_dirty() && self.edits_path.is_some(),
            save_hint: if !self.wb.is_dirty() {
                "Nothing to save — there are no unsaved edits".to_string()
            } else {
                "This sheet has no edit sidecar to save into (open a CSV to get one)".to_string()
            },
            busy,
            busy_hint: "Wait for the current operation to finish".to_string(),
            has_tables: !self.tables.is_empty(),
            has_trace: self.trace.is_some(),
            has_validation: !self.wb.validation.is_empty(),
            has_circles: self.circle_invalid,
            has_print_area: self.print_area.is_some(),
            frozen: self.panes.is_active(),
            can_undo: self.wb.can_undo(),
            can_redo: self.wb.can_redo(),
            editing: self.editing.is_some(),
            zoom: self.zoom,
            selection_label: self.selection.label(),
            rows: self.wb.view().row_count(),
            sheets: self.wb.sheet_count(),
            active_is_pivot: self.wb.is_pivot_sheet(self.wb.active_sheet()),
        }
    }

    /// Run a command, whatever invoked it, and record it as recently used.
    ///
    /// The match is exhaustive over `CommandId` by construction, so adding a
    /// registry row without giving it behaviour is a compile error rather than
    /// a menu item that does nothing.
    pub fn run_command(&mut self, id: crate::command::CommandId) {
        use crate::command::CommandId as C;
        // Recorded before dispatch: a command that opens a modal returns with
        // the app in a different state, and the ranking should reflect that
        // the user asked for it either way.
        self.command_palette.record(id);
        self.prefs.recent_commands = self.command_palette.recent_slugs();
        self.persist_prefs();
        match id {
            C::FileOpen => self.open_dialog(),
            C::FileOpenXlsx => self.open_xlsx_dialog(),
            C::FileSave => {
                let _ = self.save_edits();
            }
            C::FileCompact => self.start_compact(),
            // Issue #45. Leaving a file records where we were in it, so coming
            // back restores this position rather than the last saved one.
            C::FileOpenRecent | C::FileStartScreen => {
                self.persist_session();
                self.show_start = true;
                // Back to the launch state: the next frame behind the start
                // screen is a cold start again (issue #52).
                self.workbook_started = false;
            }
            C::FileExportCsv => self.export_dialog(),
            C::FileExportXlsx => self.export_xlsx_dialog(),
            C::FileExportParquet => self.export_parquet_dialog(),
            C::FilePrintPdf => self.print_pdf_dialog(),
            C::FilePrintHtml => self.print_html_dialog(),
            C::FileSetPrintArea => self.set_print_area(),
            C::FileClearPrintArea => self.clear_print_area(),
            C::DataValidationNew => self.validation_new_rule(),
            C::DataValidationManage => self.validation_manage(),
            C::DataValidationClear => self.validation_clear_selection(),
            C::DataCircleInvalid => self.circle_invalid_data(),
            C::DataClearCircles => self.clear_validation_circles(),
            C::DataAutocomplete => self.toggle_autocomplete(),
            C::FormatCondNew => self.cond_new_rule(),
            C::FormatCondManage => self.cond_manage(),
            C::FormatBold => {
                let on = !self.selection_typography().resolved(12.5).bold;
                self.apply_typography(move |t| t.bold = Some(on));
            }
            C::FormatItalic => {
                let on = !self.selection_typography().resolved(12.5).italic;
                self.apply_typography(move |t| t.italic = Some(on));
            }
            C::FormatUnderline => {
                let on = !self.selection_typography().resolved(12.5).underline;
                self.apply_typography(move |t| t.underline = Some(on));
            }
            C::FormatMerge => self.toggle_merge(),
            // Issue #28. These construct a `CellDecor` and hand it to
            // `apply_decor`, which is the same call the harness tests drive —
            // so the menu item and the test exercise one path, not two.
            C::FormatBorderBox => self.apply_decor(
                ferrix_core::CellDecor::default()
                    .with_box(ferrix_core::Border::new(ferrix_core::BorderStyle::Thin)),
            ),
            C::FormatBorderNone => self.apply_decor(
                ferrix_core::CellDecor::default()
                    .with_box(ferrix_core::Border::new(ferrix_core::BorderStyle::None)),
            ),
            C::FormatWrapText => {
                let on = !self.decor_at(self.selection.cursor).wrap.unwrap_or(false);
                self.apply_decor(ferrix_core::CellDecor::default().with_wrap(on));
            }
            C::FormatAlignLeft => self.apply_decor(
                ferrix_core::CellDecor::default().with_h_align(ferrix_core::HAlign::Left),
            ),
            C::FormatAlignCenter => self.apply_decor(
                ferrix_core::CellDecor::default().with_h_align(ferrix_core::HAlign::Center),
            ),
            C::FormatAlignRight => self.apply_decor(
                ferrix_core::CellDecor::default().with_h_align(ferrix_core::HAlign::Right),
            ),
            // Issue #36. Dispatch through `add_sparkline`, which is the same
            // method the harness drives -- so a test asserts through the
            // registry and the paint loop rather than around them.
            C::FormatSparkLine => self.add_sparkline(ferrix_core::SparkKind::Line),
            C::FormatSparkColumn => self.add_sparkline(ferrix_core::SparkKind::Column),
            C::FormatSparkWinLoss => self.add_sparkline(ferrix_core::SparkKind::WinLoss),
            C::FormatSparkClear => self.clear_sparklines(),
            C::FormulaTracePrecedents => self.trace_precedents(),
            C::FormulaTraceDependents => self.trace_dependents(),
            C::FormulaTraceClear => self.clear_trace(),
            C::FormulaNames => self.names_open = true,
            C::DataGoalSeek => self.goal_seek_open(),
            // Issue #17. These go through the same selection-span methods the
            // harness tests drive, so the menu item and the test exercise one
            // path rather than two.
            C::DataInsertRow => self.insert_rows_at_selection(),
            C::DataDeleteRow => self.delete_rows_at_selection(),
            C::DataInsertColumn => self.insert_columns_at_selection(),
            C::DataDeleteColumn => self.delete_columns_at_selection(),
            C::DataChart => self.open_chart(),
            // Issue #34. These call the SAME methods the harness drives, so a
            // test that goes through `run_command` exercises the production
            // dispatch path rather than a parallel entry point.
            C::DataRemoveDuplicates => self.remove_duplicates(),
            C::DataSubtotals => self.toggle_subtotals(),
            C::DataConsolidate => self.consolidate_sheets(),
            C::DataRefreshPivot => self.refresh_active_pivot(),
            C::DataLockCells => self.lock_selection(),
            C::DataUnlockCells => self.unlock_selection(),
            C::DataProtectSheet => self.protect_sheet_open(),
            C::DataProtectWorkbook => self.protect_workbook_open(),
            C::ViewFreezeRows => self.freeze_at_cursor(true, false),
            C::ViewFreezeCols => self.freeze_at_cursor(false, true),
            C::ViewFreezeBoth => self.freeze_at_cursor(true, true),
            C::ViewUnfreeze => self.unfreeze(),
            C::ViewSplit => self.split_at_cursor(),
            C::ViewZoomIn => self.zoom_in(),
            C::ViewZoomOut => self.zoom_out(),
            C::ViewZoomReset => self.zoom_reset(),
            C::ViewTheme => self.set_theme(self.theme.mode.toggled()),
            C::ViewEmptyRows => self.set_show_empty_rows(!self.show_empty_rows),
            C::ViewShowFormulas => self.toggle_show_formulas(),
            C::ViewPageBreaks => self.toggle_page_breaks(),
            C::ViewInsertPageBreak => self.insert_page_break_at_cursor(),
            C::ViewRemovePageBreak => self.remove_page_break_at_cursor(),
            C::ViewResetPageBreaks => self.reset_page_breaks(),
            C::EditUndo => {
                if let Some(c) = self.wb.undo() {
                    self.selection.move_to(c);
                    self.scroll_to_selection();
                    self.sync_formula_bar();
                }
            }
            C::EditRedo => {
                if let Some(c) = self.wb.redo() {
                    self.selection.move_to(c);
                    self.scroll_to_selection();
                    self.sync_formula_bar();
                }
            }
            C::EditSelectAll => self.select_all(),
            // Issue #30. Opening the dialog does not paste; the request is
            // assembled first and applied when the user confirms it.
            C::EditPasteSpecial => self.paste_special_open(),
            C::EditFind => {
                self.search_open = true;
                self.focus = Focus::Search;
                self.search_focus_pending = true;
            }
            C::EditReplace => {
                self.search_open = true;
                self.replace_open = true;
                self.focus = Focus::Search;
                self.replace_focus_pending = true;
            }
        }
    }

    /// Keyboard + drawing for the palette, called once per frame.
    ///
    /// Returns true when the palette consumed this frame's keys, so the grid's
    /// own handler stands down. Opening deliberately does NOT commit or cancel
    /// an in-progress cell edit: the edit buffer, the editing cell and the
    /// selection are all left exactly as they were.
    fn command_palette_frame(&mut self, ctx: &egui::Context) -> bool {
        use crate::command::PaletteKey;
        let st = self.command_state();
        let mut consumed = false;
        match self.command_palette.keys(ctx, &st) {
            PaletteKey::None => {}
            PaletteKey::Open => {
                self.command_palette.open(self.selection);
                consumed = true;
            }
            PaletteKey::Consumed => consumed = true,
            PaletteKey::Close => {
                // Escape restores the selection the palette opened over and
                // hands the keyboard back to the grid for real — egui would
                // otherwise keep focus on the hidden search field and swallow
                // every subsequent grid chord.
                if let Some(sel) = self.command_palette.close(true) {
                    self.selection = sel;
                }
                if self.editing.is_none() {
                    if let Some(fid) = ctx.memory(|m| m.focused()) {
                        ctx.memory_mut(|m| m.surrender_focus(fid));
                    }
                    self.focus = Focus::Grid;
                }
                consumed = true;
            }
            PaletteKey::Run(id) => {
                self.command_palette.close(false);
                if let Some(fid) = ctx.memory(|m| m.focused()) {
                    ctx.memory_mut(|m| m.surrender_focus(fid));
                }
                if self.editing.is_none() {
                    self.focus = Focus::Grid;
                }
                self.run_command(id);
                consumed = true;
            }
        }
        // Drawn after the key pass so a click and a keypress in the same frame
        // agree about which list they are acting on.
        let st = self.command_state();
        let th = self.theme;
        if let Some(id) = self.command_palette.show(ctx, &th, &st) {
            self.command_palette.close(false);
            self.run_command(id);
            consumed = true;
        }
        consumed || self.command_palette.is_open()
    }

    /// Is the palette open? Observation API for the harness, like the rest of
    /// the `#[allow(dead_code)]` block above — the running app never asks.
    #[allow(dead_code)]
    pub fn command_palette_is_open(&self) -> bool {
        self.command_palette.is_open()
    }

    /// The palette itself, for tests asserting on the ranked list.
    #[allow(dead_code)]
    pub fn command_palette_state(&self) -> &crate::command::CommandPalette {
        &self.command_palette
    }

    #[allow(dead_code)]
    pub fn command_palette_state_mut(&mut self) -> &mut crate::command::CommandPalette {
        &mut self.command_palette
    }

    /// The in-progress cell edit, if any, as (cell, buffer).
    ///
    /// Observation only. Exists so a test can assert that opening the palette
    /// leaves an edit strictly alone — the state that would otherwise be
    /// silently committed or discarded.
    #[allow(dead_code)]
    pub fn editing_for_test(&self) -> Option<(CellRef, String)> {
        self.editing.map(|c| (c, self.edit_buffer.clone()))
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

    /// eframe's own shutdown hook — the last point at which the process is
    /// still alive on a normal quit. Belt and braces alongside the
    /// close-request path, since a platform quit (Cmd+Q, session logout) can
    /// reach here without a viewport close request ever being observed.
    fn on_exit(&mut self) {
        self.on_clean_exit();
    }
}

impl FerrixApp {
    /// One frame of the app. This is the real update path; `eframe::App`
    /// delegates to it, and the headless harness calls it directly.
    pub fn frame(&mut self, ctx: &egui::Context) {
        self.poll_load();
        self.poll_export();
        self.poll_compact();
        // Refresh the memory reading at most once a second. Sampling is a
        // syscall; doing it per frame at 60fps would be measurable for no
        // benefit, and the number does not move meaningfully faster than that.
        self.budget = ferrix_core::budget::cached();
        if self.loading || self.exporting || self.compacting {
            ctx.request_repaint();
        }
        // Interaction-aware repainting. The app is otherwise reactive (it
        // redraws only when an event arrives), which leaves two visible
        // artefacts: a drag lags a frame behind the pointer, and a transient
        // overlay (the selection rectangle, a hover highlight) lingers until
        // the next unrelated event repaints it away. While the pointer is held
        // OVER THE APP'S CONTENT we repaint every frame so drags track live;
        // for a short settle window after any pointer or key input we keep
        // repainting so transient state clears promptly. When nothing is
        // happening we fall back to reactive mode and cost no CPU.
        //
        // The "over the content" guard matters for window-move jitter: dragging
        // the title bar counts as a button being down, but the pointer is then
        // in the OS non-client area, NOT over the content. Repainting the grid
        // every frame during the OS's modal window-move loop paints each frame
        // at a position one frame stale relative to where the window has
        // already moved, which reads as the content juddering behind the frame.
        // So an in-app drag repaints live; a window move is left entirely to
        // the OS.
        let screen = ctx.screen_rect();
        let (dragging_content, had_input) = ctx.input(|i| {
            let any_down = i.pointer.any_down();
            // The pointer position egui last saw, in content coordinates. During
            // a title-bar drag this is outside `screen` (or absent), so the
            // continuous-repaint arm below does not fire.
            let over_content = i.pointer.latest_pos().is_some_and(|p| screen.contains(p));
            let dragging_content = any_down && over_content;
            let had_input = any_down
                || i.pointer.velocity() != egui::Vec2::ZERO
                || !i.events.is_empty()
                || i.pointer.is_moving();
            (dragging_content, had_input)
        });
        // Recorded so a test can assert the window-move-jitter guard directly:
        // this is THE app's own decision to drive continuous repaints, which is
        // true only for an in-content drag and false during an OS window move.
        self.dragging_content = dragging_content;
        if dragging_content {
            ctx.request_repaint();
        } else if had_input {
            // Settle window: long enough to flush a selection/hover change and
            // any short animation, short enough to idle out quickly afterwards.
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
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

        // --- start screen (issue #45) ---
        //
        // Shown INSTEAD of the toolbar/grid on a cold launch with no file, and
        // returned to by File > Start screen. It owns the whole frame, so it
        // returns early rather than painting behind the grid.
        if self.show_start {
            if let Some(choice) = crate::recent::show_start_screen(ctx, &mut self.prefs, &th) {
                self.take_start_choice(choice);
            }
            return;
        }

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
        } else if ctx.input(|i| i.viewport().close_requested()) {
            // The close is going through — either nothing is dirty or the
            // user already answered the prompt. Either way this is a clean
            // exit, and a clean exit leaves no autosave behind.
            self.on_clean_exit();
        }

        if self.close_prompt {
            self.show_close_prompt(ctx);
        }
        // The compact modal outranks everything below it: the file under the
        // grid is being replaced, so nothing else should be reachable.
        if self.compacting {
            self.show_compact_modal(ctx);
        }
        // Offer to recover a previous session's autosave. Shown after the
        // close prompt so an in-progress close keeps precedence.
        if self.recovery.is_some() {
            self.show_recovery_prompt(ctx);
        }
        // The overwrite confirmation for a drag-drop that would clobber data
        // (#82). Shown here so it sits above the grid but below a close/compact.
        if self.pending_block_move.is_some() {
            self.show_block_move_prompt(ctx);
        }
        // The autosave timer. Runs every frame; almost every call returns
        // immediately without touching the disk.
        self.tick_autosave();
        self.show_chart_window(ctx);
        self.show_cell_menu(ctx);
        self.show_comment_editor(ctx);
        if self.cond.is_some() {
            self.show_cond_editor(ctx);
        }
        if self.validation.is_some() {
            self.show_validation_editor(ctx);
        }
        {}

        // The command palette (issue #40) sees the keyboard first: while it is
        // open it owns the keys, and its open chord must not also reach the
        // grid. Everything else about the app's state — including an edit in
        // progress — is left untouched.
        if !self.command_palette_frame(ctx) {
            self.handle_keys(ctx);
        }

        // --- toolbar ---
        //
        // Menu and toolbar choices are recorded and acted on after the panel
        // closes: the closure holds `&mut self` fields, so dispatching in
        // place would conflict with the `th` the same frame is painting with.
        // One variable for all of them now that they are all registry
        // commands (issue #40).
        let mut chosen_command: Option<crate::command::CommandId> = None;
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::none().fill(th.panel).inner_margin(8.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FERRIX").color(th.accent).strong().size(15.0));
                    ui.add_space(12.0);
                    if ui.button("Open CSV…").clicked() {
                        chosen_command = Some(crate::command::CommandId::FileOpen);
                    }
                    if ui
                        .button("⬈ Export CSV…")
                        .on_hover_text("Write this sheet, including edits, to a CSV file")
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::FileExportCsv);
                    }
                    if ui
                        .button("📈 Chart…")
                        .on_hover_text("Chart the selected range")
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::DataChart);
                    }
                    ui.separator();
                    if ui
                        .button("⬓ Merge")
                        .on_hover_text("Merge the selection, or unmerge it if already merged")
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::FormatMerge);
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
                        chosen_command = Some(crate::command::CommandId::FileOpenXlsx);
                    }
                    if ui
                        .add_enabled(!self.tables.is_empty(), egui::Button::new("⬈ Export xlsx…"))
                        .on_hover_text(
                            "Write this sheet and its table as a real Excel Table, with \
                             dataValidation, conditionalFormatting, and autoFilter parts",
                        )
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::FileExportXlsx);
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
                        chosen_command = Some(crate::command::CommandId::FileSave);
                    }
                    if ui
                        .add_enabled(self.wb.can_undo(), egui::Button::new("↶ Undo"))
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::EditUndo);
                    }
                    if ui
                        .add_enabled(self.wb.can_redo(), egui::Button::new("↷ Redo"))
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::EditRedo);
                    }
                    ui.add_space(4.0);
                    // --- theme toggle (issue #19) ---
                    if ui
                        .button(self.theme.mode.toggle_label())
                        .on_hover_text("Switch between light and dark. Remembered between runs.")
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::ViewTheme);
                    }
                    // --- empty rows toggle (issue #20) ---
                    if ui
                        .selectable_label(self.showing_formulas(), "ƒ Formulas")
                        .on_hover_text(
                            "Ctrl+` — show formula source instead of values, for this sheet",
                        )
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::ViewShowFormulas);
                    }
                    if ui
                        .selectable_label(self.show_empty_rows, "⬓ Empty rows")
                        .on_hover_text(
                            "Show empty rows past the end of the sheet so there is \
                             somewhere to type. They are not data: exports, SUM and \
                             the row count ignore them until you type in one.",
                        )
                        .clicked()
                    {
                        chosen_command = Some(crate::command::CommandId::ViewEmptyRows);
                    }
                    // --- menu bar (issue #40) ---
                    //
                    // Every menu is drawn from command::REGISTRY, the same
                    // table the palette searches. This used to be five
                    // hand-written closures, which is exactly how a command
                    // ends up in a menu and nowhere else. Availability, and
                    // the reason for it, come from the registry too, so a
                    // greyed item explains itself in both front-ends.
                    let cmd_state = self.command_state();
                    for menu in crate::command::Menu::ALL {
                        ui.menu_button(menu.title(), |ui| {
                            if let Some(id) = crate::command::menu_items(ui, menu, &cmd_state) {
                                chosen_command = Some(id);
                            }
                        });
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

        // One dispatch point for every menu item and toolbar button, so a
        // command invoked by clicking is recorded in the palette's recency
        // exactly as one invoked from the palette is.
        if let Some(id) = chosen_command {
            self.run_command(id);
        }

        // --- formula bar ---
        //
        // Issue #38: expandable to multiple lines, with a drag handle along
        // its bottom edge. The panel's height is derived from
        // `formula_bar_rows`, which is persisted, so the size the user chose
        // survives a restart.
        let bar_rows = self.formula_bar_rows;
        let row_px = ctx
            .fonts(|f| f.row_height(&egui::FontId::monospace(13.0)))
            .max(12.0);
        // Set by the drag handle inside the panel closure, applied after it
        // closes: `set_formula_bar_rows` takes `&mut self`, which the closure
        // already holds borrowed.
        let mut drag_rows: Option<usize> = None;
        let _ = row_px * bar_rows as f32 + 10.0;
        let formula_panel = egui::TopBottomPanel::top("formula_bar")
            .frame(
                egui::Frame::none()
                    .fill(th.header_bg)
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // --- Name Box ---
                    //
                    // Sits at the top-left, above the row headers, and is
                    // width-matched to them so it reads as their heading. It
                    // shows the selection's name or its A1 label, navigates to
                    // an existing name or address, and defines a new name for
                    // the selection otherwise.
                    let mut buf = self.name_box_text();
                    let resp = ui.add_sized(
                        [crate::grid::ROW_HEADER_WIDTH, 22.0],
                        egui::TextEdit::singleline(&mut buf)
                            .font(egui::TextStyle::Monospace)
                            .text_color(th.accent),
                    );
                    if resp.changed() {
                        self.name_box_edit = Some(buf);
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        self.commit_name_box();
                        self.focus = Focus::Grid;
                    } else if resp.lost_focus() {
                        // Abandoned without Enter: drop the buffer so the box
                        // snaps back to showing the live selection.
                        self.name_box_edit = None;
                    }
                    if resp.gained_focus() {
                        self.focus = Focus::FormulaBar;
                    }
                    resp.on_hover_text(
                        "Name Box — type a name to go to it, or a new name to define it \
                         for the selection",
                    );
                    if ui.small_button("▾").on_hover_text("Name Manager").clicked() {
                        self.names_open = true;
                    }
                    ui.separator();
                    ui.label(RichText::new("fx").color(th.text_dim).italics());

                    let fid = egui::Id::new(FORMULA_BAR_ID);
                    let bar_w = ui.available_width() * 0.5;
                    // ONE id for both shapes, so the caret survives the bar
                    // being expanded mid-formula.
                    //
                    // A multi-line bar keeps Enter as COMMIT rather than
                    // "insert a newline": Enter is how every spreadsheet
                    // commits a formula, and a bar that swallowed it would
                    // strand the user in a field they cannot leave. Alt+Enter
                    // inserts the newline, matching Excel.
                    let resp = if bar_rows > 1 {
                        ui.add_sized(
                            [bar_w, row_px * bar_rows as f32],
                            egui::TextEdit::multiline(&mut self.formula_input)
                                .id(fid)
                                .hint_text("=SUM(E1:E10000000)")
                                .font(egui::TextStyle::Monospace)
                                .desired_rows(bar_rows)
                                .return_key(egui::KeyboardShortcut::new(
                                    egui::Modifiers::ALT,
                                    Key::Enter,
                                )),
                        )
                    } else {
                        ui.add_sized(
                            [bar_w, 22.0],
                            egui::TextEdit::singleline(&mut self.formula_input)
                                .id(fid)
                                .hint_text("=SUM(E1:E10000000)")
                                .font(egui::TextStyle::Monospace),
                        )
                    };
                    if resp.gained_focus() {
                        self.focus = Focus::FormulaBar;
                        // The Escape snapshot for a bar-only edit. Taken on
                        // FOCUS, which is the moment "before the edit" means.
                        self.edit_pre_text.clone_from(&self.formula_input);
                    }
                    if resp.changed() {
                        self.recompute_formula();
                        // A cell edit mirrored into the bar has to flow back,
                        // or typing in the bar mid-edit would be discarded on
                        // commit.
                        if self.editing.is_some() {
                            self.edit_buffer.clone_from(&self.formula_input);
                        }
                    }
                    // Caret read-back. F4 acts on the reference the user is
                    // parked on, so it needs egui's OWN cursor rather than a
                    // guess; and a rewrite that moved the caret installs the
                    // new position here.
                    if resp.has_focus() && self.editing.is_none() {
                        if let Some(mut st) = egui::TextEdit::load_state(ctx, fid) {
                            if let Some(want) = self.pending_caret.take() {
                                let ch = byte_to_char(&self.formula_input, want);
                                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                                    egui::text::CCursor::new(ch),
                                )));
                                st.clone().store(ctx, fid);
                                self.edit_caret = want;
                            } else if let Some(r) = st.cursor.char_range() {
                                self.edit_caret =
                                    char_to_byte(&self.formula_input, r.primary.index);
                            }
                        }
                    }
                    // Escape abandons a bar edit and puts the text back.
                    if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                        if self.editing.is_some() {
                            self.cancel_edit();
                        } else {
                            self.cancel_formula_bar();
                        }
                        if let Some(id) = ctx.memory(|m| m.focused()) {
                            ctx.memory_mut(|m| m.surrender_focus(id));
                        }
                    }
                    // Enter in the formula bar commits it to the selected cell.
                    else if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
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

                // --- drag handle (issue #38) ---
                //
                // A full-width strip along the bar's bottom edge. Dragging it
                // down adds rows, up removes them. The row count is derived
                // from the pointer's CURRENT distance from the bar's top
                // rather than accumulated per frame, so a fast drag cannot
                // drift out of step with the pointer.
                let handle = ui
                    .allocate_response(egui::vec2(ui.available_width(), 6.0), egui::Sense::drag());
                let bright = handle.hovered() || handle.dragged();
                ui.painter().hline(
                    handle.rect.x_range(),
                    handle.rect.center().y,
                    egui::Stroke::new(
                        if bright { 2.0_f32 } else { 1.0_f32 },
                        if bright { th.accent } else { th.grid_line },
                    ),
                );
                if handle.hovered() || handle.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                }
                if handle.dragged() {
                    if let Some(p) = ui.ctx().pointer_interact_pos() {
                        let top = ui.min_rect().top();
                        let rows = ((p.y - top) / row_px).round().max(1.0) as usize;
                        drag_rows = Some(rows);
                    }
                }
                // Double-clicking the handle snaps between one row and a
                // comfortable multi-line height, so the expansion does not
                // require a steady hand.
                if handle.double_clicked() {
                    drag_rows = Some(if bar_rows > 1 { 1 } else { 4 });
                }
            });
        // Real layout output: the height the panel ACTUALLY occupied. A test
        // asserting the bar grew reads this rather than the row count, so a
        // stored number that never reaches layout fails.
        self.last_formula_bar_h = formula_panel.response.rect.height();
        if let Some(rows) = drag_rows {
            self.set_formula_bar_rows(rows);
        }

        // --- Name Manager ---
        if self.names_open {
            self.show_name_manager(ctx);
        }

        // --- Goal Seek (issue #35) ---
        self.show_goal_seek(ctx);
        // After Goal Seek so the wizard, being modal to the whole open, is
        // the last window drawn and therefore on top.
        self.show_import_wizard(ctx);

        // --- Protect Sheet / Workbook (issue #42) ---
        self.show_protect_dialog(ctx);

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
                        if ui
                            .selectable_label(self.search_regex, ".*")
                            .on_hover_text(
                                "Regular expression. In Replace, $1 refers to a capture group.",
                            )
                            .clicked()
                        {
                            self.search_regex = !self.search_regex;
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
                            if ui
                                .selectable_label(self.replace_open, "⇄ Replace")
                                .on_hover_text("Find and replace (Ctrl+H)")
                                .clicked()
                            {
                                self.replace_open = !self.replace_open;
                                if self.replace_open {
                                    self.replace_focus_pending = true;
                                }
                            }
                        });
                    });

                    // A malformed pattern must say so. Finding nothing looks
                    // identical to "no matches", which is the worst possible
                    // answer to a typo.
                    if let Some(e) = &self.search_regex_error {
                        ui.label(
                            RichText::new(format!("⚠ bad pattern: {e}"))
                                .color(th.error)
                                .size(11.5),
                        );
                    }

                    // --- replace row (Ctrl+H) ---
                    if self.replace_open {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("⇄").size(13.0));
                            let resp = ui.add_sized(
                                [260.0, 22.0],
                                egui::TextEdit::singleline(&mut self.replace_input)
                                    .hint_text("Replace with…")
                                    .font(egui::TextStyle::Monospace),
                            );
                            if self.replace_focus_pending {
                                resp.request_focus();
                                self.replace_focus_pending = false;
                            }
                            if resp.gained_focus() {
                                self.focus = Focus::Search;
                            }

                            // Look in: values or formula source. The default is
                            // values, because rewriting a formula's SOURCE is a
                            // much bigger hammer than most replaces want.
                            let look = self.replace_look_in;
                            ui.label(RichText::new("in").color(th.text_dim).size(11.5));
                            if ui
                                .selectable_label(look == ferrix_core::LookIn::Values, "values")
                                .on_hover_text(
                                    "Rewrite displayed values. Formula cells are skipped: \
                                     their displayed text is a computed result.",
                                )
                                .clicked()
                            {
                                self.replace_look_in = ferrix_core::LookIn::Values;
                            }
                            if ui
                                .selectable_label(look == ferrix_core::LookIn::Formulas, "formulas")
                                .on_hover_text(
                                    "Rewrite the underlying text: a formula's SOURCE \
                                     (=A1*2), not the number it currently shows.",
                                )
                                .clicked()
                            {
                                self.replace_look_in = ferrix_core::LookIn::Formulas;
                            }

                            ui.separator();
                            if ui
                                .button("Replace")
                                .on_hover_text("Replace the current match, then advance")
                                .clicked()
                            {
                                self.replace_current();
                            }
                            if ui
                                .button("Replace All")
                                .on_hover_text("Replace every match — one undo step")
                                .clicked()
                            {
                                self.replace_all();
                            }

                            // Progress + cancel for a long pass. Cancelling
                            // KEEPS what was already applied and says how much.
                            if let Some((examined, applied)) = self.replace_progress {
                                ui.label(
                                    RichText::new(format!(
                                        "{} examined · {} replaced",
                                        fmt_int(examined),
                                        fmt_int(applied)
                                    ))
                                    .color(th.text_dim)
                                    .size(11.0),
                                );
                                if ui
                                    .button("Cancel")
                                    .on_hover_text(
                                        "Stop here. Cells already replaced stay replaced.",
                                    )
                                    .clicked()
                                {
                                    self.replace_cancel.cancel();
                                }
                            }
                        });
                    }
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
                        // Trace arrow badge (roadmap #39). Same discipline as
                        // the invalid-cell badge above: when the arrow list
                        // was capped, say "showing N of M" rather than
                        // reporting the capped number as if it were the
                        // whole truth. A cell with 500k dependents must not
                        // silently look like it has 100.
                        if self.trace.is_some() {
                            let (drawn, total) = self.trace_counts();
                            let text = if total > drawn {
                                format!("↗ showing {drawn} of {total} arrows")
                            } else {
                                format!("↗ {drawn} arrow{}", plural(drawn))
                            };
                            ui.label(RichText::new(text).color(th.accent).size(11.5).monospace());
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

        // The header hide/unhide menu floats over the grid, so it is shown
        // before the CentralPanel paints underneath it.
        self.show_header_menu(ctx);

        // --- grid ---
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(th.bg))
            .show(ctx, |ui| {
                // NOTHING to open, and nothing to type into either — this is
                // the cold-start state, before any workbook exists. A sheet
                // the user CREATED is a different thing: it has no rows but
                // it must still be a usable grid, so it goes to the grid
                // below with viewport padding (issue #52). Returning early
                // for it is what made a new sheet un-typeable.
                if self.is_cold_start() {
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
                self.last_grid_rect = Some(outer);
                self.last_viewport_h =
                    outer.height() - crate::grid::Metrics::new(self.zoom).header_h;

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

                // Read before the view borrow: `showing_formulas` needs
                // `&self`, and the Grid below holds `&mut self.scroll`.
                let show_formulas_now = self.showing_formulas();
                let pad_rows_now = self.pad_rows();
                let blank_cols_now = self.navigable_cols();
                let resp = {
                    let view = self.wb.view();
                    // The conditional-formatting editor's LIVE PREVIEW. `None`
                    // whenever no dialog is previewing, and the grid then
                    // paints the real store with no clone at all.
                    let preview_fmt = self.cond_preview_format();
                    // Table decoration is prepared once per frame for the
                    // visible rows only, so its cost is independent of how
                    // many rows the table covers.
                    let decor = self.tables.first().map(|t| {
                        let first = self.scroll.row_offset.floor().max(0.0) as u32;
                        let count = (self.last_viewport_h
                            / crate::grid::Metrics::new(self.zoom).row_h)
                            .ceil() as u32
                            + 1
                            + self.panes.rows as u32;
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
                        extra_selections: &self.extra_selections,
                        col_widths: &self.col_widths,
                        scroll: &mut self.scroll,
                        editing: self.editing,
                        matches: &self.search_results.matches,
                        filling: self.fill_source.is_some(),
                        moving: self.move_origin.is_some(),
                        header_dragging: self.header_drag,
                        filter: self.row_filter.as_ref(),
                        sort: self.sort_order.as_ref(),
                        subtotals: self.subtotals.as_ref(),
                        table: decor.as_ref(),
                        current_match: if self.search_open {
                            self.search_results.wrapped(self.search_index)
                        } else {
                            None
                        },
                        theme: th,
                        format: Some(preview_fmt.as_ref().unwrap_or(&self.wb.format)),
                        merges: Some(&self.wb.merges),
                        comments: Some(&self.wb.comments),
                        pad_rows: pad_rows_now,
                        blank_cols: blank_cols_now,
                        metrics: crate::grid::Metrics::new(self.zoom),
                        panes: &mut self.panes,
                        hidden_cols: Some(&self.hidden_cols),
                        hidden_rows: self.hidden_rows.as_ref(),
                        row_outline: Some(&self.sizing.row_outline),
                        col_resizing: self.col_resize,
                        show_formulas: show_formulas_now,
                        validation: (!self.wb.validation.is_empty()).then_some(&self.wb.validation),
                        sparklines: Some(&self.wb.sparklines),
                    }
                    .show(ui)
                };

                self.last_painted = resp.painted_cells;
                self.last_comment_markers = resp.comment_markers;
                self.last_border_segments = resp.border_segments;
                self.last_rotated_texts = resp.rotated_texts;
                self.last_wrapped_texts = resp.wrapped_texts;
                // The in-cell dropdown arrow's real geometry (issue #41), so
                // a click on it opens the list at the place it was drawn
                // rather than at a constant that drifts when a bar opens.
                self.dropdown_button = resp.dropdown_button;
                self.last_subtotal_rows = resp.subtotal_rows;
                self.last_subtotal_texts = resp.subtotal_texts;
                self.last_sparkline_shapes = resp.sparkline_shapes;
                self.last_sparkline_blanks = resp.sparkline_blanks;

                // --- trace precedents / dependents arrows (roadmap #39) ---
                //
                // Painted straight onto the grid's own Painter, like the
                // in-cell editor is positioned below: the grid owns no
                // widget per cell to hang an overlay off, so this reads the
                // same geometry (`cell_screen_rect`) rather than keeping a
                // second copy of it.
                if let Some(trace) = self.trace {
                    let pad = self.pad_space();
                    let resolver = self.row_resolver(pad);
                    let metrics = crate::grid::Metrics::new(self.zoom);
                    let (edges, total) = crate::trace::edges_for(&self.wb.graph, trace);
                    let active_sheet = self.wb.active_sheet();
                    let painter = ui.painter().with_clip_rect(outer);
                    let mut drawn = 0usize;
                    for edge in &edges {
                        // Only the active sheet's endpoints are on screen at
                        // all; a cross-sheet edge is real in the model but
                        // has nowhere to be painted here.
                        if edge.from.sheet != active_sheet || edge.to.sheet != active_sheet {
                            continue;
                        }
                        let from_rect = Grid::cell_screen_rect(
                            edge.from.cell,
                            outer,
                            &self.scroll,
                            &self.col_widths,
                            &resolver,
                            metrics,
                            self.panes,
                        );
                        let to_rect = Grid::cell_screen_rect(
                            edge.to.cell,
                            outer,
                            &self.scroll,
                            &self.col_widths,
                            &resolver,
                            metrics,
                            self.panes,
                        );
                        // A cell in a cycle is drawn with a distinct dashed
                        // stroke — the existing cycle detection already
                        // finds these, so this is purely a paint choice.
                        let cyclic = self.wb.graph.is_circular_at(edge.from)
                            || self.wb.graph.is_circular_at(edge.to);
                        let color = if cyclic { th.error } else { th.accent };
                        match (from_rect, to_rect) {
                            (Some(fr), Some(tr)) => {
                                draw_trace_arrow(&painter, fr.center(), tr.center(), color, cyclic);
                            }
                            // One (or both) endpoints are off screen. Point
                            // an arrow at the viewport edge nearest the
                            // hidden endpoint, rather than at wrong
                            // coordinates or not drawing anything at all.
                            (Some(fr), None) => {
                                let edge_pt = clamp_to_rect_edge(outer, fr.center());
                                draw_trace_arrow(&painter, fr.center(), edge_pt, color, cyclic);
                            }
                            (None, Some(tr)) => {
                                let edge_pt = clamp_to_rect_edge(outer, tr.center());
                                draw_trace_arrow(&painter, edge_pt, tr.center(), color, cyclic);
                            }
                            (None, None) => continue,
                        }
                        drawn += 1;
                    }
                    self.last_trace_arrows = drawn;
                    self.last_trace_total = total;
                } else {
                    self.last_trace_arrows = 0;
                    self.last_trace_total = 0;
                }

                // --- Page Break Preview (roadmap #37, drag #76) ---
                //
                // A dashed line at every row and column where a printed page
                // would break. The break positions come from the SAME
                // `Paginator` the PDF/HTML export uses, so what the user
                // previews is exactly where the print splits. When the preview
                // is on, a line can be GRABBED and dragged to force a manual
                // break at a different row/column (Excel's Page Break Preview);
                // the grab/drop coordinates are turned into row/col via
                // `cell_at_point` and handed to `move_row_break`/`move_col_break`,
                // which own the meaning and are tested without a pointer.
                if self.show_page_breaks {
                    let extent = match self.print_area {
                        Some(r) => ((r.first_row, r.last_row), (r.first_col, r.last_col)),
                        None => {
                            let rows = self.wb.view().row_count().max(1) as u32 - 1;
                            let cols = self.wb.view().col_count().max(1) as u32 - 1;
                            ((0, rows), (0, cols))
                        }
                    };
                    let paginator = ferrix_core::page::Paginator::new(
                        self.page_setup.clone(),
                        extent.0,
                        extent.1,
                        &self.sizing.rows,
                        &self.sizing.cols,
                    );
                    let pad = self.pad_space();
                    let resolver = self.row_resolver(pad);
                    let metrics = crate::grid::Metrics::new(self.zoom);
                    let painter = ui.painter().with_clip_rect(outer);
                    let mut lines = 0usize;

                    // On-screen break lines, kept so the pointer can hit-test
                    // against them for the drag. (screen_coord, break_row/col).
                    let mut row_lines: Vec<(f32, u32)> = Vec::new();
                    let mut col_lines: Vec<(f32, u32)> = Vec::new();

                    // A row break falls BEFORE `row`: a horizontal dashed line
                    // along the top edge of that row's cells.
                    for row in paginator.row_break_rows() {
                        if let Some(rect) = Grid::cell_screen_rect(
                            ferrix_core::CellRef::new(row, extent.1 .0),
                            outer,
                            &self.scroll,
                            &self.col_widths,
                            &resolver,
                            metrics,
                            self.panes,
                        ) {
                            let y = rect.top();
                            draw_page_break_line(
                                &painter,
                                egui::pos2(outer.left(), y),
                                egui::pos2(outer.right(), y),
                                th.accent,
                            );
                            row_lines.push((y, row));
                            lines += 1;
                        }
                    }
                    // A column break falls BEFORE `col`: a vertical dashed line
                    // along the left edge of that column's cells.
                    for col in paginator.col_break_cols() {
                        if let Some(rect) = Grid::cell_screen_rect(
                            ferrix_core::CellRef::new(extent.0 .0, col),
                            outer,
                            &self.scroll,
                            &self.col_widths,
                            &resolver,
                            metrics,
                            self.panes,
                        ) {
                            let x = rect.left();
                            draw_page_break_line(
                                &painter,
                                egui::pos2(x, outer.top()),
                                egui::pos2(x, outer.bottom()),
                                th.accent,
                            );
                            col_lines.push((x, col));
                            lines += 1;
                        }
                    }
                    self.last_page_break_lines = lines;

                    // --- drag interaction (#76) ---
                    //
                    // Only MANUAL breaks can be dragged (an automatic break has
                    // no stored position to move); a manual break line is
                    // grabbable within a few pixels. Editing suppresses it so a
                    // text drag inside the editor is not stolen.
                    if self.editing.is_none() {
                        self.handle_break_drag(ctx, outer, &row_lines, &col_lines);
                    }
                } else {
                    self.last_page_break_lines = 0;
                    self.break_drag = None;
                }

                // --- Circle Invalid Data (issue #41) ---
                //
                // Painted onto the grid's own Painter, through the SAME
                // `cell_screen_rect` the trace arrows and the in-cell editor
                // use — a second copy of the geometry would drift the moment a
                // freeze or a filter is active.
                //
                // Bounded by the viewport by construction, twice over: the set
                // is computed from the rows the grid PAINTED, and
                // `cell_screen_rect` returns None for anything off screen. A
                // 200M-row sheet where every row is invalid rings a screenful.
                self.last_validation_circles = 0;
                if self.circle_invalid && !self.circled.is_empty() {
                    let pad = self.pad_space();
                    let resolver = self.row_resolver(pad);
                    let metrics = crate::grid::Metrics::new(self.zoom);
                    let painter = ui.painter().with_clip_rect(outer);
                    let mut drawn = 0usize;
                    for cell in &self.circled {
                        let Some(r) = Grid::cell_screen_rect(
                            *cell,
                            outer,
                            &self.scroll,
                            &self.col_widths,
                            &resolver,
                            metrics,
                            self.panes,
                        ) else {
                            continue;
                        };
                        if !r.intersect(outer).is_positive() {
                            continue;
                        }
                        // An ellipse inscribed in the cell, the way Excel
                        // draws it. Stroked, never filled: the value inside
                        // has to stay readable.
                        painter.add(egui::Shape::ellipse_stroke(
                            r.center(),
                            egui::vec2(r.width() * 0.46, r.height() * 0.42),
                            egui::Stroke::new(1.6_f32, th.invalid_flag),
                        ));
                        drawn += 1;
                    }
                    self.last_validation_circles = drawn;
                }

                // --- coloured reference highlighting (issue #38) ---
                //
                // Every reference in the formula being edited gets an outline
                // over the range it points at, colour-matched to its position
                // in the formula. Painted onto the grid's own Painter, the way
                // the trace arrows above are, and through the SAME
                // `cell_screen_rect` — a second copy of the geometry would
                // drift the moment a freeze or a filter is active.
                //
                // Bounded by the viewport by construction: `cell_screen_rect`
                // returns None for anything off screen, so a reference to
                // `A1:A200000000` paints one rectangle clipped to the window
                // rather than two hundred million.
                self.ref_outlines.clear();
                let spans = self.live_ref_spans();
                if spans.is_empty() {
                    self.ref_drag = None;
                } else {
                    let pad = self.pad_space();
                    let resolver = self.row_resolver(pad);
                    let metrics = crate::grid::Metrics::new(self.zoom);
                    let painter = ui.painter().with_clip_rect(outer);
                    let mut hit: Option<usize> = None;
                    let ptr = ui.ctx().pointer_interact_pos();
                    // Collected, then stored after the resolver's borrow of
                    // `self` ends.
                    let mut drawn: Vec<(egui::Rect, egui::Color32)> = Vec::new();
                    for (i, sp) in spans.iter().enumerate() {
                        let (r0, c0, r1, c1) = sp.bounds();
                        let tl = Grid::cell_screen_rect(
                            CellRef::new(r0, c0),
                            outer,
                            &self.scroll,
                            &self.col_widths,
                            &resolver,
                            metrics,
                            self.panes,
                        );
                        let br = Grid::cell_screen_rect(
                            CellRef::new(r1, c1),
                            outer,
                            &self.scroll,
                            &self.col_widths,
                            &resolver,
                            metrics,
                            self.panes,
                        );
                        // Either corner alone is enough to place a rectangle:
                        // a range half off screen still shows the half the
                        // user can see.
                        let rect = match (tl, br) {
                            (Some(a), Some(b)) => a.union(b),
                            (Some(a), None) => a,
                            (None, Some(b)) => b,
                            (None, None) => continue,
                        };
                        let rect = rect.intersect(outer);
                        if !rect.is_positive() {
                            continue;
                        }
                        let color = th.ref_colors[i % th.ref_colors.len()];
                        painter.rect_stroke(rect, 1.0, egui::Stroke::new(1.5_f32, color));
                        drawn.push((rect, color));
                        if ptr.is_some_and(|p| rect.contains(p)) {
                            // Later spans win: an outline drawn on top is the
                            // one the user is pointing at.
                            hit = Some(i);
                        }
                    }
                    self.ref_outlines = drawn;

                    // --- dragging an outline rewrites its reference ---
                    //
                    // Keyed on PRESS, not click. egui reports `primary_clicked`
                    // on release, and keying a drag off it is exactly the bug
                    // that made the fill handle silently do nothing.
                    let (pressed, down) = ui
                        .ctx()
                        .input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down()));
                    if pressed {
                        if let (Some(i), Some(p)) = (hit, ptr) {
                            if let Some(cell) = self.cell_at_point(p, outer) {
                                self.ref_drag = Some(RefDrag {
                                    span: i,
                                    from: cell,
                                });
                            }
                        }
                    } else if !down {
                        if let Some(d) = self.ref_drag.take() {
                            if let Some(to) = ptr.and_then(|p| self.cell_at_point(p, outer)) {
                                let d_row = i64::from(to.row) - i64::from(d.from.row);
                                let d_col = i64::from(to.col) - i64::from(d.from.col);
                                self.move_reference(d.span, d_row, d_col);
                            }
                        }
                    }
                }

                // Right-click opens the cell menu. Anchored at the click
                // point and remembered across frames, because the click event
                // is gone by the frame the menu is first drawn.
                if let Some((cell, pos)) = resp.context_click {
                    self.open_cell_menu(cell, pos);
                }

                // The hover tooltip. The grid paints onto a raw Painter and
                // has no widget Response, so the popup is built here from the
                // cell it reported. Drawn as a real egui tooltip so it floats
                // above the grid and follows the platform's own styling.
                if let Some(cell) = resp.hovered_comment {
                    if let Some(c) = self.wb.comments.get(cell) {
                        let text = if c.author.is_empty() {
                            c.text.clone()
                        } else {
                            format!("{}:\n{}", c.author, c.text)
                        };
                        egui::show_tooltip_text(
                            ui.ctx(),
                            ui.layer_id(),
                            egui::Id::new("ferrix_comment_tip"),
                            text,
                        );
                    }
                }
                // Where the headers really landed this frame, so a caller can
                // click one without guessing at pixels that move whenever a
                // bar above the grid opens.
                self.header_hitboxes = resp.header_hitboxes.clone();
                self.row_header_hitboxes = resp.row_header_hitboxes.clone();
                self.last_painted_rows = resp.painted_rows.clone();
                self.last_frozen_rows = resp.frozen_row_count;
                // The grid clamps the zoom (a band taller than the window is
                // refused), so the app adopts what was ACTUALLY painted rather
                // than what it asked for.
                self.zoom = resp.zoom;

                if let Some(cell) = resp.clicked {
                    // Clicking the in-cell dropdown arrow (issue #41) opens
                    // the list rather than merely re-selecting the cell: an
                    // arrow that does nothing when clicked is worse than no
                    // arrow. Keyed off the rectangle the grid actually
                    // PAINTED, so it cannot drift from what is on screen.
                    let hit_arrow = self
                        .dropdown_button
                        .zip(ui.ctx().pointer_interact_pos())
                        .is_some_and(|((c, r), p)| c == cell && r.contains(p));
                    if hit_arrow {
                        self.open_validation_dropdown(cell);
                    }
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
                // --- column resize / autofit / hide, outline (issue #29) ---
                //
                // Each branch calls the plain method the harness tests drive
                // directly. The gesture supplies coordinates; the method owns
                // the meaning.
                if let Some((c, x, w)) = resp.resize_started {
                    self.col_resize = Some((c, x, w));
                }
                if let Some((c, w)) = resp.resize_to {
                    // Live preview in the status bar while the drag is in
                    // flight; the width is only committed on release.
                    self.status = format!(
                        "Column {} width {:.0}px",
                        ferrix_core::column_name(c as u32),
                        w.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH)
                    );
                }
                if resp.resize_released {
                    // Committed from the app's OWN drag state plus this
                    // frame's pointer, so the release frame — on which egui
                    // has already cleared `is_dragging` — still lands.
                    if let (Some((c, _, _)), Some((_, w))) = (self.col_resize, resp.resize_to) {
                        self.set_col_width(c, w);
                        self.status = format!(
                            "Column {} resized to {:.0}px",
                            ferrix_core::column_name(c as u32),
                            self.col_width(c)
                        );
                    }
                    self.col_resize = None;
                }
                if let Some(c) = resp.col_autofit {
                    self.autofit_column(c);
                }
                if let Some((c, pos)) = resp.header_context {
                    self.header_menu = Some((c, pos));
                }
                if let Some(row) = resp.outline_toggle {
                    self.toggle_row_group(row);
                }
                self.last_outline_buttons = resp.outline_buttons;
                // --- header reorder ---
                //
                // Press starts the drag and selects the whole column, so the
                // user sees what they grabbed. Release commits the move.
                if let Some(c) = resp.header_press {
                    self.header_drag = Some(c);
                    // Plain / Ctrl / Shift all go through the SAME method the
                    // row case uses, so the two axes cannot drift apart.
                    let mods = ui.input(|i| i.modifiers);
                    self.select_column(c as u32, mods);
                }
                // --- row header selection (issue #17) ---
                //
                // The mirror of the column press above. Reported on PRESS with
                // the modifiers captured on that frame, because egui's
                // aggregate `i.modifiers` is only correct while the frame that
                // produced the press is still being handled.
                if let Some((row, mods)) = resp.row_header_press {
                    if self.editing.is_some() {
                        self.commit_edit();
                    }
                    self.select_row(row, mods);
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
                        if src == dst {
                            // Released on the column it was pressed on: that is
                            // a CLICK, not a reorder, and a click on a header
                            // cycles that column's sort asc -> desc -> none.
                            //
                            // Keyed on release rather than press on purpose:
                            // pressing must be free to become a drag, and
                            // sorting on press would fire a full sort every
                            // time the user started to move a column.
                            let additive = ui.input(|i| i.modifiers.shift);
                            self.header_drag = None;
                            self.sort_by_column(src, additive);
                        } else {
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
                // --- block move-drag (#82) ---
                //
                // Press on the selection border grabs the block; release drops
                // it, and the delta from the grabbed cell to the drop cell is
                // the move. One `move_selection_block` call → one undo step.
                if let Some(origin) = resp.move_started {
                    self.move_origin = Some(origin);
                }
                if resp.move_released {
                    if let (Some(origin), Some(drop)) = (self.move_origin, resp.move_to) {
                        let d_row = drop.row as i64 - origin.row as i64;
                        let d_col = drop.col as i64 - origin.col as i64;
                        // Ctrl held at the drop copies instead of moving (#82).
                        if resp.move_copy {
                            self.copy_selection_block(d_row, d_col);
                        } else {
                            self.move_selection_block(d_row, d_col);
                        }
                    }
                    self.move_origin = None;
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
                    let resolver = self.row_resolver(pad);
                    if let Some(rect) = Grid::cell_screen_rect(
                        cell,
                        outer,
                        &self.scroll,
                        &self.col_widths,
                        &resolver,
                        crate::grid::Metrics::new(self.zoom),
                        self.panes,
                    ) {
                        let id = egui::Id::new(CELL_EDITOR_ID);
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
                            self.pending_caret = None;
                        }
                        // Caret read-back for the in-cell editor, so F4 acts
                        // on the reference the user is parked on there too.
                        if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
                            if let Some(want) = self.pending_caret.take() {
                                let ch = byte_to_char(&self.edit_buffer, want);
                                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                                    egui::text::CCursor::new(ch),
                                )));
                                st.clone().store(ctx, id);
                                self.edit_caret = want;
                            } else if let Some(r) = st.cursor.char_range() {
                                self.edit_caret = char_to_byte(&self.edit_buffer, r.primary.index);
                            }
                        }
                        // The bar mirrors the cell editor while an edit is
                        // live (issue #38), so a long formula is readable in
                        // the expanded bar even when the column is narrow.
                        if edit.changed() {
                            self.formula_input.clone_from(&self.edit_buffer);
                            self.recompute_formula();
                        }
                        let enter = child.input(|i| i.key_pressed(Key::Enter));
                        let esc = child.input(|i| i.key_pressed(Key::Escape));
                        let tab = child.input(|i| i.key_pressed(Key::Tab));
                        let down = child.input(|i| i.key_pressed(Key::ArrowDown));
                        let up = child.input(|i| i.key_pressed(Key::ArrowUp));

                        // --- in-cell autocomplete (issue #41) ---
                        //
                        // Refreshed from the CURRENT buffer, after the
                        // TextEdit has taken this frame's keystrokes, so the
                        // popup reflects what the user has actually typed
                        // rather than lagging one character behind.
                        self.refresh_autocomplete();
                        if let Some(picked) = crate::validation_panel::show_autocomplete(
                            ctx,
                            &self.autocomplete,
                            rect,
                            th,
                        ) {
                            self.edit_buffer.clone_from(&picked);
                            self.formula_input = picked;
                            self.autocomplete.dismiss();
                            self.autocomplete.dismissed = false;
                            self.recompute_formula();
                        }
                        let popup_open = self.autocomplete.is_open();
                        if popup_open && (down || up) {
                            self.autocomplete.move_highlight(if down { 1 } else { -1 });
                        } else if esc {
                            // ESCAPE ORDER MATTERS. While the popup is open,
                            // Escape closes IT and leaves the typed text
                            // exactly as it stands; only a second Escape
                            // abandons the edit. Cancelling the edit on the
                            // first Escape would discard what the user typed
                            // as the price of closing a suggestion list they
                            // never asked for.
                            if !self.dismiss_autocomplete() {
                                self.cancel_edit();
                            }
                        } else if enter {
                            if popup_open && self.autocomplete.from_list {
                                // A validation dropdown's Enter picks the
                                // highlighted allowed value; committing the
                                // partial text would only be rejected.
                                self.accept_suggestion();
                            } else {
                                self.commit_edit();
                                self.move_selection(1, 0);
                            }
                        } else if tab {
                            if popup_open && self.accept_suggestion() {
                                // Tab accepted the suggestion; the selection
                                // stays put so the user can keep editing.
                            } else {
                                self.commit_edit();
                                self.move_selection(0, 1);
                            }
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
/// egui's caret is a CHAR index; `refscan`/`refedit` work in BYTES.
///
/// Two tiny conversions rather than one shared "position" type, because the
/// two libraries genuinely disagree and hiding that behind a type is how an
/// off-by-one on a formula with a non-ASCII sheet name gets written.
fn char_to_byte(s: &str, ch: usize) -> usize {
    s.char_indices().nth(ch).map(|(i, _)| i).unwrap_or(s.len())
}

fn byte_to_char(s: &str, byte: usize) -> usize {
    s.char_indices().take_while(|(i, _)| *i < byte).count()
}

fn build_workbook(
    base: BaseData,
    sheet_name: String,
    first_formulas: Option<ferrix_core::EditOverlay>,
    restored: Option<ferrix_core::EditOverlay>,
    extras: Vec<(String, BaseData, ferrix_core::EditOverlay)>,
    names: ferrix_formula::NameTable,
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
    // Names must be in place BEFORE the recalc below: a formula reading
    // `=SUM(Sales)` only resolves once the table knows what Sales is, and a
    // recalc without it would cache #NAME? into every one of them.
    let has_names = !names.is_empty();
    wb.names = names;
    // Formula cells arrive with their source and a cached value computed
    // elsewhere; rebuild the graph and recompute so nothing drifts. This is
    // also what wires up cross-sheet references between the sheets just
    // loaded — until every sheet exists, `Sheet2!A1` has nothing to resolve to.
    if restored_any || had_formulas || added > 0 || has_names {
        wb.rebuild_graph_and_recalc();
    }
    wb
}

/// The plural "s", for status lines that count things.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Parse `A1` or `A1:B10` as a selection. `None` for anything else.
///
/// Lets the Name Box double as a "go to" box, which is how every spreadsheet's
/// Name Box works — and it must be tried BEFORE defining a name, or typing
/// `B7` would create a name that the tokenizer could never resolve anyway.
fn parse_a1_selection(text: &str) -> Option<Selection> {
    let text = text.trim().replace('$', "");
    match text.split_once(':') {
        Some((a, b)) => {
            let a = CellRef::from_a1(a.trim())?;
            let b = CellRef::from_a1(b.trim())?;
            Some(Selection::new(a, b))
        }
        None => Some(Selection::single(CellRef::from_a1(&text)?)),
    }
}

/// "later" / "earlier", for the rule-reorder status message. Later WINS.
fn pos_word(delta: isize) -> &'static str {
    if delta > 0 {
        "later (now wins over the rules above it)"
    } else {
        "earlier (now loses to the rules below it)"
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

/// Status line for a completed paste (issue #30).
///
/// Says what actually happened rather than a generic "Pasted": which mode ran,
/// how many cells and format rectangles it wrote, and any caveat. A test can
/// assert on the numbers here, which a bare "Pasted" could never support.
fn paste_status(
    report: &crate::workbook::PasteReport,
    opts: ferrix_core::clipboard::PasteOptions,
) -> String {
    let mut s = if !report.col_widths.is_empty() {
        format!(
            "Pasted column widths to {} column{}",
            fmt_int(report.col_widths.len()),
            if report.col_widths.len() == 1 {
                ""
            } else {
                "s"
            }
        )
    } else if report.cells_written == 0 && report.format_rects > 0 {
        format!(
            "Pasted formatting to {} region{}",
            fmt_int(report.format_rects),
            if report.format_rects == 1 { "" } else { "s" }
        )
    } else if report.cells_written == 0 {
        "Pasted nothing — every cell was skipped".to_string()
    } else {
        format!("Pasted {} cells", fmt_int(report.cells_written))
    };
    if opts.is_special() {
        s.push_str(&format!(" · {}", opts.describe()));
    }
    if let Some(note) = &report.note {
        s.push_str(&format!(" · {note}"));
    }
    s
}

/// Is this a delimited-text file, i.e. one the import wizard applies to?
///
/// A file with no extension counts: `data` from a shell pipeline is exactly
/// the case where detection earns its keep. Binary formats are excluded by
/// name because sniffing them would offer to change the delimiter of a zip
/// archive.
fn is_delimited_path(path: &Path) -> bool {
    match path.extension().map(|e| e.to_string_lossy().to_lowercase()) {
        None => true,
        Some(ext) => !matches!(
            ext.as_str(),
            "xlsx" | "xls" | "xlsm" | "parquet" | "pq" | "arrow" | "feather" | "ferrix"
        ),
    }
}

/// Open a file, choosing storage based on size.
///
/// Small files parse straight into RAM. Large ones are converted once into the
/// columnar `.ferrix` format beside the source and then memory-mapped, so the
/// dataset is bounded by disk rather than memory and later opens are instant.
fn load_any<F, C>(
    path: &Path,
    opts: CsvOptions,
    mut progress: F,
    should_cancel: &mut C,
) -> LoadResult
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

    // Parquet and Arrow IPC route through the SAME dispatch as csv/xlsx —
    // extending this match rather than adding a second open path is what
    // keeps `File > Open`, drag-and-drop, and the recent-files list all
    // agreeing about what a `.parquet` is.
    if let Some(fmt) = ferrix_io::format_for_path(path) {
        return load_arrow(path, fmt, &mut progress);
    }

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Sheet1".to_string());

    if !ferrix_io::should_use_mmap(path) {
        let (sheet, stats) = load_csv(path, opts).map_err(|e| e.to_string())?;
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
        let restored_edits = restore_edits(path, rows as u64, cols as u32);
        return Ok(Loaded {
            rows,
            cols,
            col_widths: widths,
            summary,
            base: BaseData::Memory(sheet),
            sheet_name: stem,
            extra_sheets: Vec::new(),
            first_formulas: None,
            // A delimited file carries no protection: the concept exists only
            // in the xlsx package (issue #42).
            protection: Vec::new(),
            wb_protection: ferrix_core::WorkbookProtection::default(),
            edits_path: restored_edits.path,
            fingerprint: restored_edits.fingerprint,
            comments_path: restored_edits.comments_path,
            comments: restored_edits.comments,
            // An in-RAM sheet has no columnar cache, so there is nothing to
            // compact into. Compact is an out-of-core feature by definition.
            cache_path: None,
            restored: restored_edits.overlay,
            edit_warning: restored_edits.warning,
            recovery: restored_edits.recovery,
            names: ferrix_formula::NameTable::new(),
        });
    }

    // Large file: use (or build) the columnar cache next to the source.
    let cache = ferrix_io::cache_path_for(path);
    let reused = ferrix_io::cache_is_fresh(path, &cache);
    let mut convert_note = String::new();

    if !reused {
        // The out-of-core converter takes the full import settings, including
        // the encoding the wizard resolved (issue #65). It used to take only
        // the delimiter and header flag, which made a non-UTF-8 file decode
        // correctly or not depending on its SIZE.
        let stats = ferrix_io::convert_csv_opts(
            path,
            &cache,
            ferrix_io::ConvertOptions {
                delimiter: opts.delimiter,
                has_headers: opts.has_headers,
                encoding: opts.encoding,
            },
            &mut progress,
            should_cancel,
        )
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
    if let Some(h) = read_header_line(path, opts.delimiter, opts.encoding) {
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
    let restored_edits = restore_edits(&cache, rows as u64, cols as u32);

    Ok(Loaded {
        rows,
        cols,
        col_widths: widths,
        summary,
        base: BaseData::Mapped(Box::new(mapped)),
        sheet_name: stem,
        extra_sheets: Vec::new(),
        first_formulas: None,
        // As above: no protection concept in a delimited source.
        protection: Vec::new(),
        wb_protection: ferrix_core::WorkbookProtection::default(),
        edits_path: restored_edits.path,
        fingerprint: restored_edits.fingerprint,
        comments_path: restored_edits.comments_path,
        comments: restored_edits.comments,
        cache_path: Some(cache),
        restored: restored_edits.overlay,
        edit_warning: restored_edits.warning,
        recovery: restored_edits.recovery,
        names: ferrix_formula::NameTable::new(),
    })
}

/// Open a Parquet or Arrow IPC file.
///
/// Storage is chosen the same way a CSV's is, and for the same reason: a
/// Parquet file that would not fit in RAM is streamed into the columnar
/// `.ferrix` cache and memory-mapped, so what bounds the open is disk rather
/// than memory. The difference from CSV is only *which* streaming converter
/// runs — `ferrix_io::convert_parquet` instead of `convert_csv` — because the
/// scale rule is a property of the app, not of the CSV parser.
///
/// Arrow IPC always takes the in-RAM path: the format is designed to be
/// mapped whole and there is no partial-read story worth the complexity until
/// someone shows up with an `.arrow` file that needs one.
fn load_arrow<F>(path: &Path, fmt: ferrix_io::ArrowFormat, progress: &mut F) -> LoadResult
where
    F: FnMut(u64, u64),
{
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Sheet1".to_string());

    // Large Parquet: stream into the columnar cache and map it, exactly as a
    // large CSV does.
    if fmt == ferrix_io::ArrowFormat::Parquet && ferrix_io::should_use_mmap(path) {
        let cache = ferrix_io::cache_path_for(path);
        let reused = ferrix_io::cache_is_fresh(path, &cache);
        let mut note = String::new();
        if !reused {
            let stats = ferrix_io::convert_parquet(path, &cache, &mut *progress)
                .map_err(|e| e.to_string())?;
            note = format!(
                "converted {} rows in {:.0}s · ",
                fmt_int(stats.rows as usize),
                stats.millis as f64 / 1000.0,
            );
        }
        let mapped = ferrix_io::MappedSheet::open(&cache).map_err(|e| e.to_string())?;
        let widths = compute_col_widths_mapped(&mapped);
        let rows = mapped.row_count();
        let cols = mapped.col_count();
        let summary = format!(
            "{}{} rows × {} cols · {:.1} GB mapped from disk{}",
            note,
            fmt_int(rows),
            cols,
            mapped.mapped_bytes() as f64 / 1e9,
            if reused { " (cached)" } else { "" }
        );
        let restored = restore_edits(&cache, rows as u64, cols as u32);
        return Ok(Loaded {
            rows,
            cols,
            col_widths: widths,
            summary,
            base: BaseData::Mapped(Box::new(mapped)),
            sheet_name: stem,
            extra_sheets: Vec::new(),
            first_formulas: None,
            // Parquet/Arrow carry no protection: it is an xlsx package
            // concept (issue #42).
            protection: Vec::new(),
            wb_protection: ferrix_core::WorkbookProtection::default(),
            edits_path: restored.path,
            fingerprint: restored.fingerprint,
            comments_path: restored.comments_path,
            comments: restored.comments,
            cache_path: Some(cache),
            restored: restored.overlay,
            edit_warning: restored.warning,
            recovery: restored.recovery,
            names: ferrix_formula::NameTable::new(),
        });
    }

    let imported = ferrix_io::import_any(path).map_err(|e| e.to_string())?;
    let sheet = imported.sheet;
    let st = imported.stats;
    let widths = compute_col_widths_mem(&sheet);
    let rows = sheet.row_count();
    let cols = sheet.col_count();
    let summary = format!(
        "Loaded {} rows × {} cols from {} in {} ms · {} distinct string{}",
        fmt_int(rows),
        cols,
        if fmt == ferrix_io::ArrowFormat::Parquet {
            "Parquet"
        } else {
            "Arrow"
        },
        st.millis,
        fmt_int(st.distinct_strings),
        if st.distinct_strings == 1 { "" } else { "s" },
    );
    let restored = restore_edits(path, rows as u64, cols as u32);
    Ok(Loaded {
        rows,
        cols,
        col_widths: widths,
        summary,
        base: BaseData::Memory(sheet),
        sheet_name: stem,
        extra_sheets: Vec::new(),
        // As above: no protection concept in Parquet/Arrow.
        protection: Vec::new(),
        wb_protection: ferrix_core::WorkbookProtection::default(),
        first_formulas: None,
        edits_path: restored.path,
        fingerprint: restored.fingerprint,
        comments_path: restored.comments_path,
        comments: restored.comments,
        cache_path: None,
        restored: restored.overlay,
        edit_warning: restored.warning,
        recovery: restored.recovery,
        names: ferrix_formula::NameTable::new(),
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
    // Read in a second pass: `<definedName>` lives in xl/workbook.xml, which
    // the per-sheet import never opens. A file with no names yields an empty
    // table rather than an error.
    let names = ferrix_io::import_defined_names(path).unwrap_or_default();
    // Protection lives in each sheet's own `<sheetProtection>` element and in
    // xl/workbook.xml's `<workbookProtection>` — neither of which the cell
    // import opens (issue #42). A file with none yields empty/default rather
    // than an error, so unprotected workbooks are unaffected.
    let protection: Vec<(usize, ferrix_core::SheetProtection)> = ferrix_io::import_protection(path)
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.sheet_index, p.protection))
        .collect();
    let wb_protection = ferrix_io::import_workbook_protection(path).unwrap_or_default();
    // Comments live in xl/comments*.xml, which neither calamine nor the
    // per-sheet import opens. Only the FIRST sheet's are adopted, because the
    // workbook holds one comment map for the ACTIVE sheet — the same
    // limitation `merges` has today.
    let xlsx_comments: ferrix_core::CommentMap = ferrix_io::import_comments(path)
        .map(|cs| {
            ferrix_core::CommentMap::from_iter_cells(
                cs.into_iter()
                    .filter(|c| c.sheet_index == 0)
                    .map(|c| (c.cell, c.comment)),
            )
        })
        .unwrap_or_default();
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
        protection,
        wb_protection,
        // Sidecar edits are a CSV/mmap concept: an xlsx carries its own
        // formulas, and pairing a sidecar with a re-saved workbook would be a
        // silent mismatch waiting to happen.
        edits_path: None,
        fingerprint: None,
        // No sidecar for xlsx: comments live in the package's own
        // xl/comments*.xml and are written back there on export, so a sidecar
        // beside the file would be a second, competing source of truth.
        comments_path: None,
        comments: xlsx_comments,
        cache_path: None,
        restored: None,
        edit_warning: None,
        recovery: None,
        // Defined names live in xl/workbook.xml, not in any worksheet, so
        // they are read in their own pass beside the sheet import. A file
        // with none costs one small XML scan.
        names,
    })
}

/// Look for a sidecar next to `base` and load it if it belongs to this data.
///
/// Returns the sidecar path and fingerprint regardless, so a later save knows
/// where to write even when nothing was restored.
/// What a sidecar lookup found next to a base file.
#[derive(Default)]
struct RestoredEdits {
    path: Option<PathBuf>,
    fingerprint: Option<ferrix_io::edits::BaseFingerprint>,
    overlay: Option<ferrix_core::EditOverlay>,
    warning: Option<String>,
    recovery: Option<ferrix_io::edits::RecoveryCandidate>,
    /// Comment sidecar path, always reported so a later save knows where to
    /// write even when there was nothing to read.
    comments_path: Option<PathBuf>,
    comments: ferrix_core::CommentMap,
}

fn restore_edits(base: &Path, rows: u64, cols: u32) -> RestoredEdits {
    use ferrix_io::edits;
    let fp = match edits::BaseFingerprint::of(base, rows, cols) {
        Ok(f) => f,
        // Cannot fingerprint (permissions, vanished file): saving would be
        // unsafe, so report no path rather than risk a mismatched sidecar.
        Err(_) => return RestoredEdits::default(),
    };
    let path = edits::edits_path_for(base);
    // An autosave newer than the sidecar means the last session ended without
    // a clean exit. Detected here — two `stat`s, no parsing — and offered to
    // the user rather than applied, because silently resurrecting edits is as
    // surprising as silently losing them.
    let recovery = edits::find_recovery(&path);
    let (overlay, warning) = match edits::load_edits(&path, fp) {
        Ok(Some(ov)) => (Some(ov), None),
        Ok(None) => (None, None),
        // A rejected sidecar must be surfaced. Silently continuing would look
        // like the user's saved edits simply vanished.
        Err(e) => (None, Some(e.to_string())),
    };
    // Comments load independently of the edits sidecar and of its
    // fingerprint check: they are statements about cells that survive the base
    // being regenerated. A corrupt file is dropped rather than failing the
    // whole open — the data is what the user came for.
    let comments_path = ferrix_io::comments_path_for(base);
    let comments = ferrix_io::load_comments(&comments_path)
        .ok()
        .flatten()
        .unwrap_or_default();

    RestoredEdits {
        path: Some(path),
        fingerprint: Some(fp),
        overlay,
        warning,
        recovery,
        comments_path: Some(comments_path),
        comments,
    }
}

/// A1-style label for a cell, for status messages.
fn cell_label(cell: CellRef) -> String {
    format!("{}{}", ferrix_core::column_name(cell.col), cell.row + 1)
}

/// Draw a dashed page-break line between two points (#37). Dashed so it reads
/// as a print guide, distinct from the solid grid lines.
fn draw_page_break_line(
    painter: &egui::Painter,
    from: egui::Pos2,
    to: egui::Pos2,
    color: egui::Color32,
) {
    let stroke = egui::Stroke::new(1.5_f32, color);
    // 6px dash, 4px gap, walked along the segment.
    let total = (to - from).length();
    if total <= 0.0 {
        return;
    }
    let dir = (to - from) / total;
    let (dash, gap) = (6.0_f32, 4.0_f32);
    let mut t = 0.0_f32;
    while t < total {
        let a = from + dir * t;
        let b = from + dir * (t + dash).min(total);
        painter.line_segment([a, b], stroke);
        t += dash + gap;
    }
}

/// Draw one trace arrow from `from` to `to`. A cyclic edge is dashed so it
/// reads as distinct from an ordinary precedent/dependent arrow at a glance,
/// per the acceptance criteria — the underlying cycle detection already
/// exists in `DepGraph::is_circular_at`.
fn draw_trace_arrow(
    painter: &egui::Painter,
    from: egui::Pos2,
    to: egui::Pos2,
    color: egui::Color32,
    dashed: bool,
) {
    let stroke = egui::Stroke::new(1.6_f32, color);
    if dashed {
        // egui has no built-in dashed line; approximate with short segments
        // along the vector so a cycle arrow is visually distinct without
        // pulling in a new dependency for one stroke style.
        let delta = to - from;
        let len = delta.length();
        if len < 1.0 {
            return;
        }
        let dir = delta / len;
        let dash = 6.0_f32;
        let gap = 4.0_f32;
        let mut t = 0.0_f32;
        while t < len {
            let seg_end = (t + dash).min(len);
            painter.line_segment([from + dir * t, from + dir * seg_end], stroke);
            t += dash + gap;
        }
    } else {
        painter.line_segment([from, to], stroke);
    }
    // Arrowhead at `to`: a small filled triangle pointing along the segment.
    let delta = to - from;
    if delta.length() > 1.0 {
        let dir = delta.normalized();
        let perp = egui::Vec2::new(-dir.y, dir.x);
        let head = 6.0_f32;
        let p1 = to;
        let p2 = to - dir * head + perp * (head * 0.5);
        let p3 = to - dir * head - perp * (head * 0.5);
        painter.add(egui::Shape::convex_polygon(
            vec![p1, p2, p3],
            color,
            egui::Stroke::NONE,
        ));
    }
}

/// The point on `rect`'s border nearest `target`, for pointing an arrow at
/// the viewport edge when the real endpoint has scrolled off screen — rather
/// than drawing at wrong coordinates or dropping the arrow.
fn clamp_to_rect_edge(rect: egui::Rect, target: egui::Pos2) -> egui::Pos2 {
    egui::Pos2::new(
        target.x.clamp(rect.min.x, rect.max.x),
        target.y.clamp(rect.min.y, rect.max.y),
    )
}

/// Who a new comment is attributed to.
///
/// The OS user name, because that is the only identity Ferrix has and an
/// unattributed note is less useful than a wrong-but-editable one — the author
/// field is right there in the editor. Falls back to empty, which the xlsx
/// writer renders under a placeholder rather than a blank author.
fn default_comment_author() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

/// Recover header names from the source's first line.
///
/// Reads BYTES rather than a `String` (issue #65). The old version used
/// `BufReader::read_line`, which fails with `InvalidData` on any non-UTF-8
/// byte — and the `.ok()?` turned that failure into `None`, so a windows-1252
/// file with an accented header silently fell back to column letters. It also
/// hardcoded a comma, collapsing a semicolon-delimited header into one name.
fn read_header_line(
    path: &Path,
    delimiter: u8,
    encoding: Option<&'static ferrix_io::Encoding>,
) -> Option<Vec<String>> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut raw: Vec<u8> = Vec::new();
    BufReader::new(f).read_until(b'\n', &mut raw).ok()?;

    // Decode with the chosen encoding; lossy so one bad byte never costs the
    // whole header row. `None`/UTF-8 keeps the previous behaviour bar the BOM.
    let text = match encoding {
        Some(enc) if enc.name() != "UTF-8" => enc.decode(&raw).0.into_owned(),
        _ => String::from_utf8_lossy(raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw))
            .into_owned(),
    };

    Some(
        text.trim_end()
            .split(delimiter as char)
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

/// Narrowest a column may be dragged. A column the user can drag to nothing is
/// a column they cannot find again; hiding is the deliberate way to make one
/// disappear, and it is reversible from the header menu.
pub const MIN_COL_WIDTH: f32 = 24.0;
/// Widest a single column may be, so one runaway value cannot push every other
/// column off screen.
pub const MAX_COL_WIDTH: f32 = 1200.0;

fn width_for(chars: usize) -> f32 {
    let w = chars as f32 * 7.2 + 20.0;
    if w.is_finite() {
        w.clamp(64.0, 320.0)
    } else {
        DEFAULT_COL_WIDTH
    }
}

/// Reads the group and aggregate columns for a subtotal plan, THROUGH the
/// resolver stages below subtotals (issue #34).
///
/// This is what makes subtotals compose rather than compete: a visible
/// position is turned into a data row by the SAME `RowResolver` the painter
/// uses, with only the subtotal stage removed. There is no second mapping
/// here — that is the whole point.
struct SubtotalRows<'a> {
    view: &'a crate::sheet_view::SheetView<'a>,
    below: &'a crate::grid::RowResolver<'a>,
    group_col: u32,
}

impl SubtotalRows<'_> {
    #[inline]
    fn data_row(&self, visible: usize) -> Option<u32> {
        match self.below.resolve(visible)? {
            crate::grid::ScreenRow::Data(r) => Some(r),
            // Padding and subtotal rows are not data. The plan is built over
            // `resolved_rows`, which excludes both, so this is unreachable in
            // practice and returns None rather than guessing a row.
            _ => None,
        }
    }
}

impl ferrix_core::GroupSource for SubtotalRows<'_> {
    fn group_value(&self, visible: usize) -> Value {
        match self.data_row(visible) {
            Some(r) => self.view.get(CellRef::new(r, self.group_col)),
            None => Value::Empty,
        }
    }

    fn group_label(&self, visible: usize) -> String {
        match self.data_row(visible) {
            Some(r) => self.view.display(CellRef::new(r, self.group_col)),
            None => String::new(),
        }
    }

    fn agg_value(&self, visible: usize, col: u32) -> Value {
        match self.data_row(visible) {
            Some(r) => self.view.get(CellRef::new(r, col)),
            None => Value::Empty,
        }
    }
}

/// Reads labelled ranges out of every sheet in the workbook, for Consolidate.
///
/// One call per cell of the SOURCE RECTANGLES the user selected — never a
/// sheet scan. A sheet the workbook does not have reads as empty, which is
/// then reported as a missing contribution rather than as a zero.
struct WorkbookRanges<'a> {
    wb: &'a Workbook,
}

impl WorkbookRanges<'_> {
    fn view_of(&self, sheet: &str) -> Option<crate::sheet_view::SheetView<'_>> {
        let id = self.wb.sheet_id_by_name(sheet)?;
        self.wb.sheet_view(id)
    }
}

impl ferrix_core::consolidate::RangeSource for WorkbookRanges<'_> {
    fn label_at(&self, sheet: &str, row: u32, col: u32) -> String {
        match self.view_of(sheet) {
            Some(v) => v.display(CellRef::new(row, col)),
            None => String::new(),
        }
    }

    fn number_at(&self, sheet: &str, row: u32, col: u32) -> Option<f64> {
        // `as_number` and NOT a text parse: a cell holding the WORD "5" is
        // not a number on this sheet, and quietly coercing it would make a
        // consolidation disagree with the SUM the user can see.
        self.view_of(sheet)?.get(CellRef::new(row, col)).as_number()
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

    /// Issue #42: the EXPORT the menu item runs must preserve protection.
    ///
    /// Asserts through `export_xlsx_to` — the production path — rather than
    /// through `ferrix_io::export_workbook_full`. That distinction is the
    /// whole point: the io layer's own tests passed for the entire time this
    /// method was calling `export_workbook_with_names`, which writes no
    /// `<sheetProtection>` at all, so a protected workbook silently exported
    /// unprotected. Re-importing is what proves the bytes carry it.
    #[test]
    fn exporting_preserves_sheet_and_workbook_protection() {
        let mut app = FerrixApp::new(None);
        app.wb.commit_edit(CellRef::new(0, 0), "1");

        let secret = ferrix_core::PasswordHash::of("hunter2");
        app.wb
            .protection_mut()
            .protect(ferrix_core::Allowances::default(), secret);
        app.wb.workbook_protection_mut().protect_structure(secret);

        let tmp = TempXlsx::new("protect-roundtrip");
        app.export_xlsx_to(tmp.path());
        assert!(
            tmp.path().exists(),
            "export did not write a file; status: {}",
            app.status
        );

        let sheets = ferrix_io::import_protection(tmp.path()).expect("re-import protection");
        let sp = sheets
            .iter()
            .find(|p| p.sheet_index == 0)
            .map(|p| &p.protection)
            .expect("the exported sheet must carry a <sheetProtection> element");
        assert!(
            sp.is_enabled(),
            "the re-imported sheet is unprotected — export stripped it"
        );
        assert_eq!(
            sp.hash(),
            secret,
            "the password hash must survive the round trip"
        );

        let wbp =
            ferrix_io::import_workbook_protection(tmp.path()).expect("re-import wb protection");
        assert!(
            wbp.structure_locked(),
            "workbook structure protection was stripped by export"
        );
    }

    /// Issue #36: the EXPORT the menu item runs must carry sparklines.
    ///
    /// Asserts through `export_xlsx_to` -- the production path -- for exactly
    /// the reason the protection test above gives. `sparkline_xlsx`'s own
    /// tests would keep passing if `export_xlsx_to` never called
    /// `.with_sparklines(..)`, and the file would come back with none.
    /// Re-importing is what proves the bytes carry it.
    #[test]
    fn exporting_preserves_sparkline_groups() {
        let mut app = FerrixApp::new(None);
        // Four numeric source columns over two rows.
        for r in 0..2u32 {
            for c in 0..4u32 {
                app.wb
                    .commit_edit(CellRef::new(r, c), &format!("{}", r * 4 + c + 1));
            }
        }
        app.set_selection_for_test(CellRef::new(0, 0), CellRef::new(1, 3));
        // Through the REGISTRY dispatch, not `add_sparkline`.
        app.run_command(crate::command::CommandId::FormatSparkColumn);
        assert_eq!(app.sparkline_group_count(), 1, "status: {}", app.status);

        let tmp = TempXlsx::new("spark-roundtrip");
        app.export_xlsx_to(tmp.path());
        assert!(
            tmp.path().exists(),
            "export did not write a file; status: {}",
            app.status
        );

        let back = ferrix_io::import_sparklines(tmp.path()).expect("re-import sparklines");
        assert_eq!(
            back.len(),
            1,
            "the re-imported workbook has no sparkline group -- export stripped it"
        );
        assert_eq!(
            back[0].group.kind,
            ferrix_core::SparkKind::Column,
            "the TYPE must survive, not just the geometry"
        );
        assert_eq!(
            back[0].group.target,
            ferrix_core::TableRange::new(0, 4, 1, 4)
        );
        assert_eq!(
            (back[0].group.src_first_col, back[0].group.src_last_col),
            (0, 3)
        );
    }

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

    /// `.parquet` must reach the grid through the SAME `load_any` dispatch
    /// csv and xlsx use — not a parallel path bolted on beside it.
    ///
    /// What would this assert if the feature did nothing? `load_any` would
    /// fall through to the CSV loader, which on Parquet's binary header
    /// produces either an error or one garbage row. So the assertions are on
    /// the decoded VALUES at specific coordinates, with their types intact.
    #[test]
    fn a_parquet_file_opens_through_the_normal_load_path() {
        let dir = std::env::temp_dir().join(format!(
            "ferrix-ui-parquet-open-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sales.parquet");

        // Build the fixture through the public exporter, so this test also
        // pins that what Ferrix writes, Ferrix reads.
        let mut src = ferrix_core::Sheet::new("sales");
        src.set_headers(vec!["region".into(), "units".into(), "active".into()]);
        for r in 0..250u32 {
            src.set_text(
                CellRef::new(r, 0),
                ["north", "south", "east"][r as usize % 3],
            );
            src.set(CellRef::new(r, 1), Value::Number(r as f64 * 2.0));
            src.set(CellRef::new(r, 2), Value::Bool(r % 2 == 0));
        }
        ferrix_io::export_parquet(
            &src,
            &path,
            &ferrix_io::ExportOptions {
                use_headers: true,
                ..Default::default()
            },
        )
        .expect("write parquet fixture");

        let loaded = load_any(&path, CsvOptions::default(), |_, _| {}, &mut || false)
            .expect("parquet must open");
        assert_eq!(loaded.rows, 250, "every row must arrive");
        assert_eq!(loaded.cols, 3);
        // The sheet is named from the file, as csv's is.
        assert_eq!(loaded.sheet_name, "sales");

        let wb = build_workbook(
            loaded.base,
            loaded.sheet_name,
            loaded.first_formulas,
            loaded.restored,
            loaded.extra_sheets,
            loaded.names,
        );
        let view = wb.view();
        // Per-row identity through the real open path, at both ends and the
        // middle — not "the status line is non-empty".
        for r in [0u32, 1, 2, 137, 249] {
            assert_eq!(
                view.display(CellRef::new(r, 0)),
                ["north", "south", "east"][r as usize % 3],
                "row {r} region"
            );
            assert_eq!(
                view.get(CellRef::new(r, 1)),
                Value::Number(r as f64 * 2.0),
                "row {r} units"
            );
            assert_eq!(
                view.get(CellRef::new(r, 2)),
                Value::Bool(r % 2 == 0),
                "row {r} active must stay a Bool, not become a number"
            );
        }
        // Headers came from the Parquet field names, not A/B/C.
        assert_eq!(view.header_or_letter(0), "region");
        assert_eq!(view.header_or_letter(1), "units");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same, for `.arrow`. Separate test because the two extensions reach
    /// different readers and a dispatch that only handled one would otherwise
    /// pass.
    #[test]
    fn an_arrow_ipc_file_opens_through_the_normal_load_path() {
        let dir = std::env::temp_dir().join(format!(
            "ferrix-ui-arrow-open-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feed.arrow");

        let mut src = ferrix_core::Sheet::new("feed");
        src.set_headers(vec!["k".into(), "v".into()]);
        for r in 0..64u32 {
            src.set_text(CellRef::new(r, 0), &format!("k{r}"));
            src.set(CellRef::new(r, 1), Value::Number(r as f64 / 8.0));
        }
        ferrix_io::export_ipc(
            &src,
            &path,
            &ferrix_io::ExportOptions {
                use_headers: true,
                ..Default::default()
            },
        )
        .expect("write arrow fixture");

        let loaded = load_any(&path, CsvOptions::default(), |_, _| {}, &mut || false)
            .expect(".arrow must open");
        assert_eq!(loaded.rows, 64);
        assert_eq!(loaded.cols, 2);

        let wb = build_workbook(
            loaded.base,
            loaded.sheet_name,
            loaded.first_formulas,
            loaded.restored,
            loaded.extra_sheets,
            loaded.names,
        );
        let view = wb.view();
        for r in [0u32, 7, 63] {
            assert_eq!(view.display(CellRef::new(r, 0)), format!("k{r}"));
            assert_eq!(view.get(CellRef::new(r, 1)), Value::Number(r as f64 / 8.0));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.parquet` whose contents are not Parquet must REPORT, not panic and
    /// not silently open as an empty sheet.
    #[test]
    fn a_corrupt_parquet_file_reports_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "ferrix-ui-badparquet-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("junk.parquet");
        std::fs::write(&path, b"this is definitely not a parquet file").unwrap();

        let err = match load_any(&path, CsvOptions::default(), |_, _| {}, &mut || false) {
            Ok(loaded) => panic!(
                "a non-Parquet .parquet must not open; got {} rows x {} cols",
                loaded.rows, loaded.cols
            ),
            Err(e) => e,
        };
        assert!(
            !err.trim().is_empty(),
            "the failure must carry a message the status bar can show"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn importing_a_multi_sheet_xlsx_populates_every_sheet() {
        let tmp = TempXlsx::new("multisheet");
        write_three_sheet_xlsx(tmp.path());

        let loaded =
            load_any(tmp.path(), CsvOptions::default(), |_, _| {}, &mut || false).expect("load");
        assert_eq!(loaded.sheet_name, "Alpha");
        assert_eq!(loaded.extra_sheets.len(), 2, "Beta and Gamma must load too");

        let wb = build_workbook(
            loaded.base,
            loaded.sheet_name,
            loaded.first_formulas,
            loaded.restored,
            loaded.extra_sheets,
            loaded.names,
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
        let loaded =
            load_any(tmp.path(), CsvOptions::default(), |_, _| {}, &mut || false).expect("load");
        let mut wb = build_workbook(
            loaded.base,
            loaded.sheet_name,
            loaded.first_formulas,
            loaded.restored,
            loaded.extra_sheets,
            loaded.names,
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
        let wb = build_workbook(
            BaseData::Memory(s),
            "data".into(),
            None,
            None,
            Vec::new(),
            ferrix_formula::NameTable::new(),
        );
        assert_eq!(wb.sheet_count(), 1);
        assert_eq!(wb.active_name(), "data");
        assert_eq!(wb.view().get(CellRef::new(0, 0)), Value::Number(5.0));
    }

    // ---- issue #65: header recovery must honour encoding and delimiter ----

    fn write_header_fixture(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn header_recovery_decodes_a_non_utf8_header_instead_of_giving_up() {
        // "id,année,prix" in windows-1252. The old `read_line` returned
        // Err(InvalidData) on the 0xE9, and `.ok()?` turned that into None —
        // so the sheet fell back to column LETTERS with no headers at all.
        let mut raw = b"id,ann".to_vec();
        raw.push(0xE9);
        raw.extend_from_slice(b"e,prix\n1,2020,5\n");
        let p = write_header_fixture("hdr_1252.csv", &raw);

        let got = read_header_line(
            &p,
            b',',
            Some(ferrix_io::encoding_for_label("windows-1252").unwrap()),
        );

        assert_eq!(
            got,
            Some(vec![
                "id".to_string(),
                "année".to_string(),
                "prix".to_string()
            ]),
            "the accented header must decode, not vanish"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn header_recovery_splits_on_the_chosen_delimiter() {
        // A semicolon file used to collapse into ONE header name because the
        // split was hardcoded to a comma.
        let p = write_header_fixture("hdr_semi.csv", b"id;name;prix\n1;a;2\n");

        let got = read_header_line(&p, b';', None).unwrap();

        assert_eq!(got, vec!["id", "name", "prix"]);
        assert_eq!(got.len(), 3, "a hardcoded comma would yield one column");
        let _ = std::fs::remove_file(&p);
    }

    /// #27 P2: starting to edit a spilled cell explains it is read-only rather
    /// than letting the refusal look like a broken keyboard. Drives the real
    /// `begin_edit` chokepoint through the app.
    #[test]
    fn editing_a_spilled_cell_says_it_is_part_of_a_spill() {
        let mut app = FerrixApp::new(None);
        app.wb.commit_edit(CellRef::new(0, 3), "10"); // D1
        app.wb.commit_edit(CellRef::new(1, 3), "20"); // D2
        app.wb.commit_edit(CellRef::new(0, 0), "=D1:D3"); // A1 host spills A1:A2

        // A2 is a spilled projection.
        assert!(app.wb.is_spilled_cell(CellRef::new(1, 0)));
        app.begin_edit_for_test(CellRef::new(1, 0), None);
        assert!(
            app.status_text().contains("part of a spilled array"),
            "status was: {}",
            app.status_text()
        );
    }

    // ---- issue #33 Part B: pivot sheet at the app layer ----

    /// A two-sheet app: Sheet1 (MAIN) is a source with region/amount, Sheet2 is
    /// the pivot. Returns the app, source id and pivot id.
    fn pivot_app() -> (FerrixApp, ferrix_core::SheetId, ferrix_core::SheetId) {
        let mut app = FerrixApp::new(None);
        let rows: [(&str, f64); 5] = [
            ("East", 10.0),
            ("West", 5.0),
            ("East", 20.0),
            ("West", 7.0),
            ("East", 3.0),
        ];
        for (r, (region, amount)) in rows.iter().enumerate() {
            app.wb.commit_edit(CellRef::new(r as u32, 0), region);
            app.wb
                .commit_edit(CellRef::new(r as u32, 1), &amount.to_string());
        }
        let source = app.wb.active_sheet();
        let pivot = app
            .wb
            .add_sheet(
                "Pivot",
                crate::sheet_view::BaseData::Memory(Sheet::new("Pivot")),
            )
            .expect("add pivot sheet");
        (app, source, pivot)
    }

    fn app_sum_spec() -> crate::workbook::PivotSpec {
        crate::workbook::PivotSpec {
            group_by: vec![ferrix_core::ColIdx(0)],
            values: vec![(ferrix_core::ColIdx(1), ferrix_core::PivotAgg::Sum)],
        }
    }

    #[test]
    fn beginning_to_edit_a_pivot_cell_says_it_is_a_pivot() {
        let (mut app, source, pivot) = pivot_app();
        app.wb.set_pivot(pivot, source, app_sum_spec());
        app.wb.refresh_pivot(pivot);
        app.wb.activate(pivot).unwrap();

        app.begin_edit_for_test(CellRef::new(1, 0), None);
        assert!(
            app.status_text().contains("pivot table cell"),
            "status was: {}",
            app.status_text()
        );
    }

    #[test]
    fn refresh_command_reports_the_group_count() {
        let (mut app, source, pivot) = pivot_app();
        app.wb.set_pivot(pivot, source, app_sum_spec());
        app.wb.activate(pivot).unwrap();

        app.run_command(crate::command::CommandId::DataRefreshPivot);
        assert!(
            app.status_text().contains("Refreshed pivot") && app.status_text().contains('2'),
            "status was: {}",
            app.status_text()
        );
    }

    #[test]
    fn pivot_spec_survives_a_sidecar_save_and_reload() {
        // Save the app's pivot binding to a real .fxpivot file, then load it
        // into a fresh app the way `poll_load` does. This exercises the app's
        // production save_pivots / load_pivots_sidecar path end to end.
        let dir = std::env::temp_dir().join(format!(
            "ferrix_pivot_app_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("book.ferrix");
        let sidecar = ferrix_io::pivot_path_for(&base);

        let (mut app, source, pivot) = pivot_app();
        app.wb.set_pivot(pivot, source, app_sum_spec());
        app.wb.set_pivot_auto_refresh(pivot, true);
        app.pivots_path = Some(sidecar.clone());
        assert!(app.save_pivots(), "save_pivots wrote the sidecar");
        assert!(sidecar.exists(), "the .fxpivot file is on disk");

        // Fresh app with the same two sheets, then load the sidecar.
        let (mut app2, source2, pivot2) = pivot_app();
        assert_eq!(source2, source, "same sheet ids in the rebuilt fixture");
        assert_eq!(pivot2, pivot);
        assert!(!app2.wb.is_pivot_sheet(pivot2), "no pivot before load");
        app2.pivots_path = Some(sidecar.clone());
        app2.load_pivots_sidecar();

        assert!(
            app2.wb.is_pivot_sheet(pivot2),
            "pivot restored from sidecar"
        );
        let b = app2.wb.pivot_binding(pivot2).expect("binding");
        assert_eq!(b.source, source2);
        assert_eq!(b.spec, app_sum_spec(), "the spec survived the round trip");
        assert!(b.auto_refresh, "the auto-refresh flag survived too");
        // And it was refreshed on load, so the cells already show a result.
        app2.wb.activate(pivot2).unwrap();
        assert_eq!(app2.wb.view().get(CellRef::new(1, 1)), Value::Number(33.0));

        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
