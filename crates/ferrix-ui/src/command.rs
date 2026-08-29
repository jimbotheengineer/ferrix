//! The command registry and the command palette (issue #40).
//!
//! ## Naming
//!
//! `palette` already means COLOUR palette in this codebase (the theme, issue
//! #19). Everything here is spelled `CommandPalette` / `command_palette` so a
//! grep for one never lands on the other.
//!
//! ## One registry, two front-ends
//!
//! The point of this module is not the search box. It is [`REGISTRY`]: a
//! single table that both the menu bar and the palette read. Before this,
//! the menu bar was five hand-written closures, so "add a command" meant
//! "add it to a menu", and anything reachable only from the toolbar or a
//! keyboard shortcut was invisible to anyone who did not already know it
//! existed.
//!
//! Now a command is one row in [`REGISTRY`]. [`menu_items`] draws a menu from
//! it and [`CommandPalette::matches`] searches it, so a command cannot appear
//! in one and be missing from the other. `menus_are_drawn_only_from_the_registry`
//! in `command/tests.rs` fails if anyone re-hardcodes a menu item in `app.rs`.
//!
//! The registry holds only *description* — id, label, shortcut text, which
//! menu, why it might be unavailable. Execution stays in `FerrixApp::run_command`,
//! which the compiler forces to stay exhaustive over [`CommandId`].
//!
//! ## Unavailable, not invisible
//!
//! A command that cannot run right now is listed DISABLED WITH THE REASON
//! rather than hidden. Hiding it makes it undiscoverable: the user searches
//! "compact", finds nothing, and concludes the feature does not exist. See
//! [`Command::disabled_reason`].
//!
//! ## Recency
//!
//! [`CommandPalette::recent`] is a most-recent-first list of ids, bumped by
//! every run through *either* front-end, and persisted to `prefs.toml` as one
//! `recent_commands = a,b,c` line. It only breaks ties on score, so typing a
//! precise query still finds the precise command.

use egui::{Key, Modifiers, RichText};
use ferrix_core::Selection;

use crate::theme::Theme;

/// How many recently used commands are remembered across a restart.
///
/// Bounded on purpose: this is a preference file, not a history log, and the
/// tail of the list has no effect on ranking anyone would notice.
pub const MAX_RECENT: usize = 40;

/// The menus on the menu bar, in bar order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Menu {
    File,
    Format,
    Formula,
    Data,
    View,
}

impl Menu {
    pub const ALL: [Menu; 5] = [
        Menu::File,
        Menu::Format,
        Menu::Formula,
        Menu::Data,
        Menu::View,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Menu::File => "File",
            Menu::Format => "Format",
            Menu::Formula => "Formula",
            Menu::Data => "Data",
            Menu::View => "View",
        }
    }
}

/// A small live value drawn above an item in a menu.
///
/// The old hand-written menus showed the selection label and the current zoom
/// inline. They are part of the menu's description, so they live in the
/// registry rather than in a special case in `app.rs`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// "Selection: B2:D9"
    Selection,
    /// "Zoom 150%"
    Zoom,
}

/// One command: everything needed to *describe* it. Nothing here runs.
#[derive(Copy, Clone, Debug)]
pub struct Command {
    pub id: CommandId,
    /// Stable key written to `prefs.toml`. Never change one: an old
    /// preferences file would silently lose that command's recency.
    pub slug: &'static str,
    /// The menu this appears in, or `None` for palette-only commands —
    /// toolbar buttons and keyboard-only actions, which is exactly the set a
    /// menu-only registry left undiscoverable.
    pub menu: Option<Menu>,
    pub title: &'static str,
    /// Rendered as typed, e.g. "Ctrl+S". `None` means no shortcut.
    pub shortcut: Option<&'static str>,
    /// Draw a separator above this item in its menu.
    pub separator_before: bool,
    pub note_before: Option<Note>,
    pub hint: &'static str,
}

/// Declares the [`CommandId`] enum and [`REGISTRY`] from one table, so a
/// variant cannot exist without a row and a row cannot exist without a
/// variant. `run_command`'s match then keeps execution exhaustive too.
macro_rules! registry {
    ($( $v:ident { $slug:literal, $menu:expr, $title:literal, $key:expr, $sep:expr, $note:expr, $hint:literal } )*) => {
        /// Every command in the app. Menus and the palette both index this.
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
        pub enum CommandId { $($v,)* }

        /// The single source of truth. Order is menu order and, for a tie on
        /// score and recency, palette order.
        pub const REGISTRY: &[Command] = &[
            $( Command {
                id: CommandId::$v,
                slug: $slug,
                menu: $menu,
                title: $title,
                shortcut: $key,
                separator_before: $sep,
                note_before: $note,
                hint: $hint,
            }, )*
        ];
    };
}

registry! {
    // ---- File ----
    FileOpen { "file.open", Some(Menu::File), "📂 Open CSV…", None, false, None,
        "Open a CSV file. Large files stream through the columnar cache rather than loading into RAM." }
    FileOpenXlsx { "file.open_xlsx", Some(Menu::File), "📂 Open xlsx…", None, false, None,
        "Open a workbook, importing any Excel Tables with their validation, formatting and filters." }
    FileSave { "file.save", Some(Menu::File), "💾 Save edits", Some("Ctrl+S"), false, None,
        "Write the edit sidecar beside the source file." }
    // Issue #45. `FileOpenRecent` opens the start screen positioned on the
    // recent list rather than being one command per remembered file: the
    // registry is a `const` table, and a per-file command cannot live in one.
    // The greyed/removable handling for unreachable entries therefore lives in
    // `recent::show_start_screen`, which is the only place that list is drawn.
    FileOpenRecent { "file.open_recent", Some(Menu::File), "🕘 Open Recent…", None, false, None,
        "Reopen a recently used file, restoring where you were in it." }
    FileStartScreen { "file.start_screen", Some(Menu::File), "🏠 Start screen…", None, false, None,
        "Recent files, a blank workbook, or a template." }
    FileCompact { "file.compact", Some(Menu::File), "🗜 Compact…", None, true, None,
        "Rewrite the columnar cache with the edits baked in, then retire the sidecar." }
    FileExportCsv { "file.export_csv", Some(Menu::File), "⬈ Export CSV…", None, true, None,
        "Write this sheet, including edits, to a CSV file." }
    FileExportXlsx { "file.export_xlsx", Some(Menu::File), "⬈ Export xlsx…", None, false, None,
        "Write this sheet and its table as a real Excel Table, with validation, conditional formatting and autoFilter." }
    FileExportParquet { "file.export_parquet", Some(Menu::File), "⬈ Export Parquet…", None, false, None,
        "Write this sheet, including edits, as a Parquet file. Columns keep their type, and the write streams one column stripe at a time." }
    FilePrintPdf { "file.print_pdf", Some(Menu::File), "🖨 Print to PDF…", None, false, None,
        "Render this sheet to a paginated PDF. Streams one page at a time; a very large sheet is refused until confirmed." }
    FilePrintHtml { "file.print_html", Some(Menu::File), "🖨 Print to HTML…", None, false, None,
        "Render this sheet to a single self-contained HTML page, one table per printed page." }

    // ---- Format ----
    FormatCondNew { "format.cond_new", Some(Menu::Format), "🎨 Conditional Formatting — New Rule…", None, false, Some(Note::Selection),
        "Colour cells by their value. Stored once per column or range, never per cell." }
    FormatCondManage { "format.cond_manage", Some(Menu::Format), "☰ Conditional Formatting — Manage Rules…", None, false, None,
        "List, reorder, edit and delete the rules here." }
    FormatBold { "format.bold", Some(Menu::Format), "𝐁 Bold", Some("Ctrl+B"), true, None,
        "Toggle bold over the selection." }
    FormatItalic { "format.italic", Some(Menu::Format), "𝐼 Italic", Some("Ctrl+I"), false, None,
        "Toggle italic over the selection." }
    FormatUnderline { "format.underline", Some(Menu::Format), "U̲ Underline", Some("Ctrl+U"), false, None,
        "Toggle underline over the selection." }
    FormatMerge { "format.merge", Some(Menu::Format), "⬓ Merge / unmerge", None, true, None,
        "Merge the selection, or unmerge it if it is already merged." }

    // ---- Format: cell decoration (issue #28) ----
    //
    // The decoration model and the paint path both landed complete, and for a
    // while nothing constructed a `CellDecor` outside the tests — the exact
    // "model-complete, unreachable" shape this repo has shipped four times.
    // These rows are the user-facing half. They cover the decorations that are
    // one unambiguous action; anything needing a value (rotation angle, indent
    // level, border colour) waits for a dialog rather than guessing a default.
    FormatBorderBox { "format.border_box", Some(Menu::Format), "▢ Box border", None, true, Some(Note::Selection),
        "Draw a thin border around the selection. Stored once per column or range, never per cell." }
    FormatBorderNone { "format.border_none", Some(Menu::Format), "▁ Clear borders", None, false, None,
        "Remove the borders from the selection." }
    FormatWrapText { "format.wrap_text", Some(Menu::Format), "↵ Wrap text", None, false, None,
        "Wrap long text inside the cell, growing the row to fit." }
    FormatAlignLeft { "format.align_left", Some(Menu::Format), "⯇ Align left", None, true, None,
        "Align the selection's text to the left of its cells." }
    FormatAlignCenter { "format.align_center", Some(Menu::Format), "⯀ Align centre", None, false, None,
        "Centre the selection's text in its cells." }
    FormatAlignRight { "format.align_right", Some(Menu::Format), "⯈ Align right", None, false, None,
        "Align the selection's text to the right of its cells." }

    // ---- Format: sparklines (issue #36) ----
    //
    // In the Format menu rather than Data: a sparkline is a way of DRAWING
    // cells the user already has, not a transformation of the data. It is
    // stored exactly like a conditional format -- one entry per range -- and
    // is drawn by the grid's paint loop rather than by a chart object, which
    // is why it belongs beside the other per-range formatting rather than
    // beside "Chart...".
    FormatSparkLine { "format.spark_line", Some(Menu::Format), "\u{2197} Sparkline: line", None, true, Some(Note::Selection),
        "Draw a tiny line chart of each selected row in the column to its right. Painted per visible row, so it costs the same on a 200M-row sheet." }
    FormatSparkColumn { "format.spark_column", Some(Menu::Format), "\u{2588} Sparkline: column", None, false, None,
        "Draw a tiny bar chart of each selected row in the column to its right." }
    FormatSparkWinLoss { "format.spark_winloss", Some(Menu::Format), "\u{00b1} Sparkline: win/loss", None, false, None,
        "Draw one equal-height bar per value, up for positive and down for negative. Magnitude is deliberately ignored." }
    FormatSparkClear { "format.spark_clear", Some(Menu::Format), "\u{2716} Remove sparklines", None, false, None,
        "Remove the sparkline groups drawing inside the selection." }

    // ---- Data: protection (issue #42) ----
    //
    // In the Data menu rather than Format: locking a cell is a statement
    // about who may change the data, not about how it looks.
    DataLockCells { "data.lock_cells", Some(Menu::Data), "🔒 Lock cells", None, true, Some(Note::Selection),
        "Mark the selection locked. Cells are locked by DEFAULT — the flag only bites once the sheet is protected." }
    DataUnlockCells { "data.unlock_cells", Some(Menu::Data), "🔓 Unlock cells", None, false, None,
        "Mark the selection editable even while the sheet is protected. This is how you leave input cells open on a protected form." }
    DataProtectSheet { "data.protect_sheet", Some(Menu::Data), "🛡 Protect Sheet…", None, false, None,
        "Refuse edits to locked cells, with granular allowances. Deters accidents; it is not encryption and does not resist anyone determined." }
    DataProtectWorkbook { "data.protect_workbook", Some(Menu::Data), "🛡 Protect Workbook Structure…", None, false, None,
        "Prevent sheets being added, deleted, renamed or reordered. Deters accidents; it is not encryption." }

    // ---- Formula ----
    FormulaTracePrecedents { "formula.trace_precedents", Some(Menu::Formula), "↖ Trace Precedents", Some("Ctrl+["), false, None,
        "Draw arrows from what this cell reads. Press again to walk one level further out." }
    FormulaTraceDependents { "formula.trace_dependents", Some(Menu::Formula), "↘ Trace Dependents", Some("Ctrl+]"), false, None,
        "Draw arrows to what reads this cell. Press again to walk one level further out." }
    FormulaTraceClear { "formula.trace_clear", Some(Menu::Formula), "✖ Remove Arrows", None, false, None,
        "Clear the trace arrows." }
    FormulaNames { "formula.names", Some(Menu::Formula), "🏷 Name Manager…", None, true, None,
        "Define, edit and scope named ranges." }

    // ---- Data: structural edits (issue #17) ----
    //
    // In the Data menu because inserting a row changes the DATA's shape, not
    // its appearance. Each acts on the selection's span, so selecting three
    // rows and choosing Insert Row inserts three.
    //
    // These are registry rows AND `run_command` arms on purpose: this repo has
    // shipped six model-complete, unreachable features, and an engine method
    // with no dispatch arm is exactly that shape. The harness tests drive
    // `run_command`, so a missing arm fails a test rather than shipping.
    DataInsertRow { "data.insert_row", Some(Menu::Data), "⬆ Insert row(s)", None, true, Some(Note::Selection),
        "Insert blank rows above the selection. Permutes the display order only — the columnar file on disk is never rewritten." }
    DataDeleteRow { "data.delete_row", Some(Menu::Data), "⌫ Delete row(s)", None, false, None,
        "Delete the selected rows. Formulas referring to them become #REF! rather than silently reading a neighbour." }
    DataInsertColumn { "data.insert_column", Some(Menu::Data), "⬅ Insert column(s)", None, false, None,
        "Insert blank columns left of the selection. Formula references follow the shift, so =SUM(B1:B10) keeps summing the same data." }
    DataDeleteColumn { "data.delete_column", Some(Menu::Data), "⌦ Delete column(s)", None, false, None,
        "Delete the selected columns. Formulas referring to them become #REF! rather than silently reading a neighbour." }

    // ---- Data ----
    // ---- Data: validation and autocomplete (issue #41) ----
    DataValidationNew { "data.validation_new", Some(Menu::Data), "✓ Data Validation — New Rule…", None, true, Some(Note::Selection),
        "Restrict what may be typed into the selection: a list, a number range, a date, a text length, or a custom formula. Stored once per range, never per cell." }
    DataValidationManage { "data.validation_manage", Some(Menu::Data), "☰ Data Validation — Manage Rules…", None, false, None,
        "List, edit and delete the validation rules on this sheet." }
    DataValidationClear { "data.validation_clear", Some(Menu::Data), "✖ Clear validation from selection", None, false, None,
        "Remove every validation rule covering the selection." }
    DataCircleInvalid { "data.circle_invalid", Some(Menu::Data), "○ Circle Invalid Data", None, false, None,
        "Ring every visible cell that fails its validation rule. Evaluated over the VIEWPORT only, so it costs the same on a 200M-row sheet." }
    DataClearCircles { "data.clear_circles", Some(Menu::Data), "✖ Clear validation circles", None, false, None,
        "Remove the circles." }
    DataAutocomplete { "data.autocomplete", Some(Menu::Data), "⌨ Suggest values while typing", None, true, None,
        "Offer matching values from the same column as you type. Suggestions come from a BOUNDED scan, never a full pass over the column." }

    DataGoalSeek { "data.goal_seek", Some(Menu::Data), "🎯 Goal Seek…", None, false, None,
        "Set a formula cell to a target value by changing one input cell. The whole run is a single undo step." }
    DataChart { "data.chart", Some(Menu::Data), "📈 Chart…", None, false, None,
        "Chart the selected range." }

    // ---- Data: issue #34 ----
    //
    // These are the user-facing half. Six features in this codebase landed
    // model-complete and unreachable because the model was finished and the
    // registry row was not, so the rows go in with the model.
    DataRemoveDuplicates { "data.remove_duplicates", Some(Menu::Data), "🧹 Remove Duplicates", None, true, Some(Note::Selection),
        "Drop rows whose selected columns repeat, keeping the FIRST of each. Streams a set of KEYS, never a copy of the data, and the whole run is one undo step." }
    DataSubtotals { "data.subtotals", Some(Menu::Data), "Σ Subtotals — group by cursor column", None, false, None,
        "Insert a subtotal at each change of value in the cursor's column. A VIEW only — no rows are inserted, sort and filter keep working, and running it again restores the original view exactly." }
    DataConsolidate { "data.consolidate", Some(Menu::Data), "⊞ Consolidate sheets…", None, false, None,
        "Aggregate the selected labelled range from every sheet by row and column key. Keys missing from a sheet are REPORTED, never silently zeroed." }

    // ---- View ----
    ViewFreezeRows { "view.freeze_rows", Some(Menu::View), "❄ Freeze rows above cursor", None, false, None,
        "Rows above the cursor stay put while the rest scrolls." }
    ViewFreezeCols { "view.freeze_cols", Some(Menu::View), "❄ Freeze columns left of cursor", None, false, None,
        "Columns left of the cursor stay put while the rest scrolls." }
    ViewFreezeBoth { "view.freeze_both", Some(Menu::View), "❄ Freeze both at cursor", None, false, None,
        "Freeze the rows above and the columns left of the cursor." }
    ViewUnfreeze { "view.unfreeze", Some(Menu::View), "✖ Unfreeze", None, false, None,
        "Release the frozen band." }
    ViewSplit { "view.split", Some(Menu::View), "⬍ Split at cursor", None, true, None,
        "Two independent scroll offsets over the same columns." }
    ViewZoomIn { "view.zoom_in", Some(Menu::View), "＋ Zoom in", Some("Ctrl+="), true, Some(Note::Zoom),
        "Step up to the next zoom stop." }
    ViewZoomOut { "view.zoom_out", Some(Menu::View), "－ Zoom out", Some("Ctrl+-"), false, None,
        "Step down to the previous zoom stop." }
    ViewZoomReset { "view.zoom_reset", Some(Menu::View), "＝ Reset zoom to 100%", Some("Ctrl+0"), false, None,
        "Back to 100%." }
    ViewTheme { "view.theme", Some(Menu::View), "◐ Switch light / dark theme", None, true, None,
        "Switch between light and dark. Remembered between runs." }
    ViewShowFormulas { "view.show_formulas", Some(Menu::View), "ƒ Show formulas", Some("Ctrl+`"), false, None,
        "Show each cell's formula source instead of its value, for this sheet. Rendered from the viewport, so it costs the same on a 200M-row sheet." }
    ViewEmptyRows { "view.empty_rows", Some(Menu::View), "⬓ Show empty rows", None, false, None,
        "Show empty rows past the end of the sheet so there is somewhere to type. They are not data: exports, SUM and the row count ignore them until you type in one." }

    // ---- palette only ----
    //
    // These have no menu. They are reachable from the toolbar or from a
    // shortcut, which is precisely the set that a menu-shaped registry would
    // leave undiscoverable — the reason the palette indexes the registry
    // rather than the menu bar.
    EditUndo { "edit.undo", None, "↶ Undo", Some("Ctrl+Z"), false, None,
        "Undo the last edit." }
    EditRedo { "edit.redo", None, "↷ Redo", Some("Ctrl+Y"), false, None,
        "Redo the last undone edit." }
    EditSelectAll { "edit.select_all", None, "▣ Select all", Some("Ctrl+A"), false, None,
        "Select the whole sheet." }
    EditFind { "edit.find", None, "🔍 Find…", Some("Ctrl+F"), false, None,
        "Search the sheet." }
    EditReplace { "edit.replace", None, "🔁 Find and Replace…", Some("Ctrl+H"), false, None,
        "Search and replace across the sheet." }
    // Issue #30. Plain Ctrl+V is a keystroke, not a command; Paste Special
    // needs a way in, and this is the one that does not require a toolbar.
    EditPasteSpecial { "edit.paste_special", None, "📋 Paste Special…", Some("Ctrl+Shift+V"), false, None,
        "Paste values, formulas, formats, column widths, transposed, or combined arithmetically with what is already there." }
}

impl Command {
    /// The registry row for an id.
    pub fn of(id: CommandId) -> &'static Command {
        REGISTRY
            .iter()
            .find(|c| c.id == id)
            .expect("every CommandId comes from REGISTRY by construction")
    }

    /// Why this command cannot run right now, in the user's terms.
    ///
    /// `None` means it can run. A greyed-out item with no explanation is
    /// indistinguishable from a bug, so every reason here is a sentence, not
    /// a flag.
    pub fn disabled_reason(&self, st: &CommandState) -> Option<String> {
        // A reason supplied by the app but left empty would render as a grey
        // row with no explanation, which is the exact failure this criterion
        // exists to prevent. Fall back to a sentence rather than to nothing.
        fn say(reason: &str, fallback: &str) -> Option<String> {
            Some(if reason.trim().is_empty() {
                fallback.to_string()
            } else {
                reason.to_string()
            })
        }
        use CommandId::*;
        match self.id {
            FileOpen | FileOpenXlsx | FileExportCsv | FileExportXlsx | FileExportParquet
            | FilePrintPdf | FilePrintHtml
                if st.busy =>
            {
                say(&st.busy_hint, "Wait for the current operation to finish")
            }
            FileCompact if !st.can_compact => {
                say(&st.compact_hint, "This sheet cannot be compacted right now")
            }
            FileSave if !st.can_save => say(&st.save_hint, "There is nothing to save right now"),
            FileExportXlsx if !st.has_tables => Some(
                "No Excel Table on this sheet — export CSV, or import a workbook that has one"
                    .to_string(),
            ),
            FormulaTraceClear if !st.has_trace => Some("No trace arrows are drawn".to_string()),
            DataValidationManage | DataValidationClear if !st.has_validation => {
                Some("No validation rules on this sheet yet".to_string())
            }
            DataCircleInvalid if !st.has_validation => {
                Some("Nothing to check — add a validation rule first".to_string())
            }
            DataClearCircles if !st.has_circles => {
                Some("No validation circles are drawn".to_string())
            }
            // Nothing to group and nothing to dedupe on an empty sheet, and a
            // consolidation of one sheet is a copy. Said out loud rather than
            // hidden: a user who searches "consolidate" and finds nothing
            // concludes the feature does not exist.
            DataRemoveDuplicates | DataSubtotals if st.rows == 0 => {
                Some("This sheet has no rows".to_string())
            }
            DataConsolidate if st.sheets < 2 => {
                Some("Consolidate needs at least two sheets — this workbook has one".to_string())
            }
            ViewUnfreeze if !st.frozen => Some("Nothing is frozen or split".to_string()),
            EditUndo if !st.can_undo => Some("Nothing to undo".to_string()),
            EditRedo if !st.can_redo => Some("Nothing to redo".to_string()),
            ViewZoomIn if st.zoom >= crate::grid::MAX_ZOOM => {
                Some("Already at the largest zoom".to_string())
            }
            ViewZoomOut if st.zoom <= crate::grid::MIN_ZOOM => {
                Some("Already at the smallest zoom".to_string())
            }
            // The type shortcuts belong to the text field while a cell is
            // open for editing, so the commands say so rather than silently
            // doing nothing.
            FormatBold | FormatItalic | FormatUnderline | FormatMerge | EditSelectAll
                if st.editing =>
            {
                Some("Finish the cell edit first".to_string())
            }
            _ => None,
        }
    }

    /// Label as drawn, with the shortcut appended the way the old menus did.
    pub fn menu_label(&self) -> String {
        match self.shortcut {
            Some(k) => format!("{}  ({k})", self.title),
            None => self.title.to_string(),
        }
    }
}

/// Everything the registry needs in order to decide what can run.
///
/// A snapshot of small scalars rather than a borrow of the app: the menu bar
/// builds it once per frame while `&mut self` is held by the panel closure,
/// which a borrow of the app could not survive.
#[derive(Clone, Debug, Default)]
pub struct CommandState {
    pub can_compact: bool,
    pub compact_hint: String,
    pub can_save: bool,
    pub save_hint: String,
    pub busy: bool,
    pub busy_hint: String,
    pub has_tables: bool,
    pub has_trace: bool,
    /// At least one sheet-range validation rule exists (issue #41).
    pub has_validation: bool,
    /// Validation circles are currently drawn.
    pub has_circles: bool,
    pub frozen: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub editing: bool,
    pub zoom: f32,
    pub selection_label: String,
    /// Rows on the active sheet, and sheets in the workbook. Both are small
    /// scalars the menu bar already has to hand, and both answer a
    /// "why is this grey" question for issue #34's commands.
    pub rows: usize,
    pub sheets: usize,
}

/// Commands belonging to one menu, in registry order.
pub fn for_menu(menu: Menu) -> impl Iterator<Item = &'static Command> {
    REGISTRY.iter().filter(move |c| c.menu == Some(menu))
}

/// Draw one menu's items straight from the registry.
///
/// This is the whole reason the palette can promise completeness: the menu bar
/// has no list of its own to drift from.
pub fn menu_items(ui: &mut egui::Ui, menu: Menu, st: &CommandState) -> Option<CommandId> {
    let mut chosen = None;
    for cmd in for_menu(menu) {
        if cmd.separator_before {
            ui.separator();
        }
        match cmd.note_before {
            Some(Note::Selection) => {
                ui.label(
                    RichText::new(format!("Selection: {}", st.selection_label))
                        .color(ui.visuals().weak_text_color())
                        .small(),
                );
                ui.separator();
            }
            Some(Note::Zoom) => {
                ui.label(format!("Zoom {}%", (st.zoom * 100.0).round() as i32));
            }
            None => {}
        }
        let reason = cmd.disabled_reason(st);
        let hover = match &reason {
            // The reason outranks the description: it answers the question the
            // user is actually asking when an item is grey.
            Some(r) => r.clone(),
            None => cmd.hint.to_string(),
        };
        let resp = ui
            .add_enabled(
                reason.is_none(),
                egui::Button::new(cmd.menu_label()).wrap_mode(egui::TextWrapMode::Extend),
            )
            .on_hover_text(&hover)
            .on_disabled_hover_text(&hover);
        if resp.clicked() {
            chosen = Some(cmd.id);
            ui.close_menu();
        }
    }
    chosen
}

// ---------------------------------------------------------------------------
// fuzzy matching
// ---------------------------------------------------------------------------

/// Score `text` against `query` as a case-insensitive subsequence.
///
/// `None` means no match at all. Higher is better. Consecutive runs and
/// word starts score heavily, which is what makes "cf" find "Conditional
/// Formatting" and "zo" prefer "Zoom out" over "Freeze columns".
///
/// An empty query matches everything at zero, so the palette opens on the full
/// list ordered purely by recency.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    if q.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.chars().collect();
    let lower: Vec<char> = hay
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();

    let mut score = 0i32;
    let mut qi = 0usize;
    let mut prev_hit: Option<usize> = None;
    for (i, &ch) in lower.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if ch != q[qi] {
            continue;
        }
        score += 1;
        // A word start is what a human means by "the c in Conditional".
        let starts_word = i == 0
            || !lower[i - 1].is_alphanumeric()
            || (hay[i].is_uppercase() && !hay[i - 1].is_uppercase());
        if starts_word {
            score += 8;
        }
        if prev_hit == Some(i.wrapping_sub(1)) {
            score += 5;
        }
        if i == 0 {
            score += 10;
        }
        prev_hit = Some(i);
        qi += 1;
    }
    if qi < q.len() {
        return None;
    }
    // Prefer matches that start early, but never enough to beat a run.
    let start = prev_hit.map(|_| 0).unwrap_or(0);
    Some(score - start)
}

// ---------------------------------------------------------------------------
// the palette
// ---------------------------------------------------------------------------

/// One row of the palette's filtered, ranked list.
#[derive(Clone, Debug)]
pub struct Match {
    pub id: CommandId,
    pub title: &'static str,
    pub shortcut: Option<&'static str>,
    pub hint: &'static str,
    pub menu: Option<Menu>,
    /// `Some(reason)` when the command cannot run right now. Still listed.
    pub disabled: Option<String>,
    pub score: i32,
    /// Position in the recency list, if it has ever been run.
    pub recent_rank: Option<usize>,
}

/// What a frame of palette keyboard input asks the app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteKey {
    /// Nothing to do; the app should carry on with its own key handling.
    None,
    /// Open the palette. The app supplies the selection to restore on Escape.
    Open,
    /// Handled here — the app must not also act on this keystroke.
    Consumed,
    /// Escape: close and restore.
    Close,
    /// Enter (or a click): run this, then close.
    Run(CommandId),
}

/// The command palette's state. `open == false` costs nothing per frame.
#[derive(Clone, Debug, Default)]
pub struct CommandPalette {
    open: bool,
    pub query: String,
    /// Index into the *filtered* list, clamped on every use.
    pub cursor: usize,
    /// True on the frame it opens, so the search field takes focus once.
    focus_pending: bool,
    /// The selection at the moment it opened, restored on Escape.
    ///
    /// Opening must not disturb the grid, and Escape must put the user back
    /// exactly where they were — including when a command was highlighted but
    /// never run.
    saved_selection: Option<Selection>,
    /// Most-recent-first. Persisted; see `Prefs::recent_commands`.
    recent: Vec<CommandId>,
}

impl CommandPalette {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open on the full list. `selection` is what Escape restores.
    pub fn open(&mut self, selection: Selection) {
        self.open = true;
        self.query.clear();
        self.cursor = 0;
        self.focus_pending = true;
        self.saved_selection = Some(selection);
    }

    /// Close. Returns the selection to restore, when the caller asked for it —
    /// which is Escape, not Enter: a command that moves the cursor (Undo,
    /// Goal Seek) must keep the cursor it moved to.
    pub fn close(&mut self, restore: bool) -> Option<Selection> {
        self.open = false;
        self.focus_pending = false;
        let saved = self.saved_selection.take();
        if restore {
            saved
        } else {
            None
        }
    }

    /// Bump a command to the front of the recency list.
    pub fn record(&mut self, id: CommandId) {
        self.recent.retain(|&r| r != id);
        self.recent.insert(0, id);
        self.recent.truncate(MAX_RECENT);
    }

    /// Read by the persistence tests; the app persists via `recent_slugs`.
    #[allow(dead_code)]
    pub fn recent(&self) -> &[CommandId] {
        &self.recent
    }

    /// Slugs for `prefs.toml`, most recent first.
    pub fn recent_slugs(&self) -> Vec<String> {
        self.recent
            .iter()
            .map(|&id| Command::of(id).slug.to_string())
            .collect()
    }

    /// Adopt a persisted recency list. Unknown slugs — a command removed since
    /// the file was written — are dropped rather than treated as an error.
    pub fn set_recent_slugs(&mut self, slugs: &[String]) {
        self.recent = slugs
            .iter()
            .filter_map(|s| REGISTRY.iter().find(|c| c.slug == s).map(|c| c.id))
            .take(MAX_RECENT)
            .collect();
    }

    /// The filtered, ranked list. Score first, recency as the tie-break, and
    /// registry order under that (the sort is stable).
    pub fn matches(&self, st: &CommandState) -> Vec<Match> {
        let q = self.query.trim();
        let mut out: Vec<Match> = REGISTRY
            .iter()
            .filter_map(|c| {
                let score = fuzzy_score(q, c.title)?;
                Some(Match {
                    id: c.id,
                    title: c.title,
                    shortcut: c.shortcut,
                    hint: c.hint,
                    menu: c.menu,
                    disabled: c.disabled_reason(st),
                    score,
                    recent_rank: self.recent.iter().position(|&r| r == c.id),
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.score.cmp(&a.score).then(
                a.recent_rank
                    .unwrap_or(usize::MAX)
                    .cmp(&b.recent_rank.unwrap_or(usize::MAX)),
            )
        });
        out
    }

    /// One frame of keyboard handling, called from the app's single key path.
    ///
    /// Keys are CONSUMED (`input_mut`), not merely read. Everything else in
    /// this app reads without consuming, but the palette must: the in-cell
    /// editor checks for Escape in the PAINT path later in the same frame, so
    /// a merely-observed Escape would close the palette *and* cancel the
    /// user's edit. Consuming is what makes "opening the palette does not
    /// disturb the current edit" true for closing it too.
    pub fn keys(&mut self, ctx: &egui::Context, st: &CommandState) -> PaletteKey {
        // Ctrl+Shift+P and Ctrl+/ both toggle. Two bindings because muscle
        // memory splits between editors, and neither collides with a grid key.
        //
        // Shift-first, per egui's own guidance: `matches_logically` ignores an
        // extra Shift, so a bare Ctrl+P check would swallow Ctrl+Shift+P.
        let toggle = ctx.input_mut(|i| {
            let shift_p = i.consume_key(
                Modifiers {
                    command: true,
                    shift: true,
                    ..Default::default()
                },
                Key::P,
            );
            let slash = i.consume_key(Modifiers::COMMAND, Key::Slash);
            shift_p || slash
        });
        if toggle {
            return if self.open {
                PaletteKey::Close
            } else {
                PaletteKey::Open
            };
        }
        if !self.open {
            return PaletteKey::None;
        }
        let (esc, enter, up, down, page_up, page_down) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::PageUp),
                i.consume_key(Modifiers::NONE, Key::PageDown),
            )
        });
        if esc {
            return PaletteKey::Close;
        }
        let list = self.matches(st);
        let len = list.len();
        if len > 0 {
            self.cursor = self.cursor.min(len - 1);
            let step = |c: usize, d: i64| -> usize {
                let n = len as i64;
                (((c as i64 + d) % n) + n) as usize % len
            };
            if down {
                self.cursor = step(self.cursor, 1);
            }
            if up {
                self.cursor = step(self.cursor, -1);
            }
            if page_down {
                self.cursor = (self.cursor + 8).min(len - 1);
            }
            if page_up {
                self.cursor = self.cursor.saturating_sub(8);
            }
            if enter {
                let m = &list[self.cursor];
                // A disabled command stays put with its reason on screen
                // rather than closing the palette and doing nothing.
                if m.disabled.is_none() {
                    return PaletteKey::Run(m.id);
                }
            }
        }
        PaletteKey::Consumed
    }

    /// Draw the palette. Returns a command when one was clicked.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        th: &Theme,
        st: &CommandState,
    ) -> Option<CommandId> {
        if !self.open {
            return None;
        }
        let list = self.matches(st);
        if !list.is_empty() {
            self.cursor = self.cursor.min(list.len() - 1);
        }
        let mut clicked = None;
        egui::Window::new("Command palette")
            .id(egui::Id::new("ferrix_command_palette"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .fixed_size([560.0, 0.0])
            .show(ctx, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Run a command…   ↑↓ to move, Enter to run, Esc to close")
                        .desired_width(f32::INFINITY),
                );
                if self.focus_pending {
                    resp.request_focus();
                    self.focus_pending = false;
                }
                if resp.changed() {
                    // A new query means a new list; keeping the old index
                    // would highlight an unrelated row.
                    self.cursor = 0;
                }
                ui.separator();
                if list.is_empty() {
                    ui.label(RichText::new("No matching command").color(th.text_dim));
                }
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        for (i, m) in list.iter().enumerate() {
                            let selected = i == self.cursor;
                            let row = ui.horizontal(|ui| {
                                let title = RichText::new(m.title).color(if m.disabled.is_some() {
                                    th.text_dim
                                } else {
                                    th.text
                                });
                                ui.label(if selected { title.strong() } else { title });
                                // The reason travels WITH the row: a grey line
                                // with no explanation is why people file bugs
                                // against working features.
                                if let Some(r) = &m.disabled {
                                    ui.label(
                                        RichText::new(format!("— {r}"))
                                            .color(th.text_dim)
                                            .italics()
                                            .small(),
                                    );
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if let Some(k) = m.shortcut {
                                            ui.label(
                                                RichText::new(k).color(th.text_dim).monospace(),
                                            );
                                        }
                                        if let Some(menu) = m.menu {
                                            ui.label(
                                                RichText::new(menu.title())
                                                    .color(th.text_dim)
                                                    .small(),
                                            );
                                        }
                                    },
                                );
                            });
                            let rect = row.response.rect;
                            if selected {
                                ui.painter().rect_stroke(
                                    rect.expand(1.0),
                                    2.0,
                                    egui::Stroke::new(1.0_f32, th.accent),
                                );
                            }
                            let hit = ui.interact(
                                rect,
                                egui::Id::new(("ferrix_cmd_row", m.id)),
                                egui::Sense::click(),
                            );
                            if hit.clicked() && m.disabled.is_none() {
                                clicked = Some(m.id);
                            }
                            if let Some(r) = &m.disabled {
                                hit.on_hover_text(r);
                            } else {
                                hit.on_hover_text(m.hint);
                            }
                        }
                    });
            });
        clicked
    }
}

#[cfg(test)]
mod tests;
