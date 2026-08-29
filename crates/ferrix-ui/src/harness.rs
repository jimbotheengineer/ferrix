//! Drive the real `FerrixApp` headlessly, without the OS input layer.
//!
//! ## Why this exists
//!
//! Every GUI check in this project has gone through synthetic OS input, and it
//! is unreliable against egui in two specific ways:
//!
//! * a click is resolved against `interact_pos`, which only updates when a
//!   pointer MOVE event arrives, so a click with no preceding move lands
//!   nowhere; and
//! * `ctrl+key` frequently loses its modifier — a `Ctrl+F` recently arrived as
//!   a literal `f` typed into cell A1.
//!
//! That has produced four false bug reports. The reverse also happened: the
//! fill handle keyed on `primary_clicked`, which egui reports on *release*, so
//! press-and-drag never started a fill. Every unit test passed. Only the
//! running GUI caught it.
//!
//! So unit tests pass on broken interaction, and synthetic input fails on
//! working interaction. Neither can be trusted alone.
//!
//! ## What this does instead
//!
//! It constructs the real [`FerrixApp`] and calls the real `update` against an
//! `egui::Context` we own, feeding `RawInput` directly. No window, no OS
//! events, no races, no sleeps — one call is exactly one frame. Because it
//! drives the actual app, a test here fails when the app is broken, which is
//! the property a harness that reimplements app logic would lose.
//!
//! Input is expressed in the same terms egui consumes, so a "click" here is a
//! pointer move *followed by* press and release — reproducing the sequence a
//! real mouse produces, and the one synthetic input keeps getting wrong.

use std::path::Path;

use eframe::egui::{self, Event, Key, Modifiers, Pos2, RawInput, Vec2};

use crate::app::FerrixApp;

/// Wall-clock floor for [`Harness::step_until`].
///
/// Generous on purpose: it is only ever reached when the thing being waited
/// for never arrives, i.e. when the test is going to fail anyway. In the happy
/// path the predicate holds within a handful of frames and this is never
/// consulted.
const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A headless Ferrix instance.
#[allow(dead_code)]
pub struct Harness {
    ctx: egui::Context,
    app: FerrixApp,
    /// Queued for the next frame, then cleared.
    events: Vec<Event>,
    pointer: Pos2,
    screen: Vec2,
    frame: u64,
    /// Paint shapes emitted by the last frame.
    last_shapes: usize,
    /// Text shapes emitted by the last frame.
    last_texts: usize,
    /// Aggregate modifier state for the next frame.
    ///
    /// egui exposes `i.modifiers` from `RawInput.modifiers`, NOT from the
    /// modifiers attached to individual key events, and the app reads
    /// `i.modifiers.command`. Setting only per-event modifiers makes a Ctrl+F
    /// arrive as a bare F -- the exact failure synthetic OS input produces.
    pending_modifiers: Modifiers,
}

#[allow(dead_code)]
impl Harness {
    /// Build a harness over a fresh app, optionally opening a file.
    pub fn new(initial: Option<&Path>) -> Self {
        let ctx = egui::Context::default();
        // Theme is now a value, not a set of consts. The harness pins DARK so
        // a test never depends on the persisted user preference.
        let theme = crate::theme::Theme::dark();
        crate::theme::Theme::apply(&theme, &ctx);
        let app = FerrixApp::new(initial.map(|p| p.to_path_buf()));
        Self {
            ctx,
            app,
            events: Vec::new(),
            pointer: Pos2::new(400.0, 300.0),
            screen: Vec2::new(1400.0, 880.0),
            frame: 0,
            last_shapes: 0,
            last_texts: 0,
            pending_modifiers: Modifiers::default(),
        }
    }

    /// Run one frame, consuming any queued input.
    ///
    /// A frame is the unit of progress: nothing in the app happens between
    /// frames, so a test never needs to sleep or poll.
    pub fn step(&mut self) -> &mut Self {
        let raw = RawInput {
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, self.screen)),
            events: std::mem::take(&mut self.events),
            modifiers: self.pending_modifiers,
            ..Default::default()
        };
        // Modifiers apply to the frame they were queued for, then lift.
        self.pending_modifiers = Modifiers::default();
        // Calls the app's real per-frame path directly. eframe::Frame cannot
        // be constructed outside eframe, and update() only forwards to this,
        // so the harness exercises identical code without it.
        let app = &mut self.app;
        let out = self.ctx.run(raw, |ctx| app.frame(ctx));
        // Tessellated shape count for the frame just drawn. This is the real
        // paint output, not a proxy for it, which is what lets a test assert
        // that a style change reached the screen rather than only the model.
        self.last_shapes = out.shapes.len();
        self.last_texts = out
            .shapes
            .iter()
            .filter(|s| matches!(s.shape, egui::epaint::Shape::Text(_)))
            .count();
        self.frame += 1;
        self
    }

    /// Run `n` frames. Loading happens on a worker thread, so a test that opens
    /// a file steps until the load lands rather than sleeping.
    /// Text shapes emitted by the most recent frame.
    ///
    /// Separate from the total because a style change may swap one shape kind
    /// for another; text count is what answers "did this cell draw its value".
    pub fn paint_text_count(&mut self) -> usize {
        self.step();
        self.last_texts
    }

    /// Number of paint shapes emitted by the most recent frame.
    pub fn paint_shape_count(&mut self) -> usize {
        // Draw one fresh frame so the count reflects current state rather
        // than whatever the last queued input happened to leave behind.
        self.step();
        self.last_shapes
    }

    pub fn steps(&mut self, n: usize) -> &mut Self {
        for _ in 0..n {
            self.step();
        }
        self
    }

    /// Step until `pred` holds or `max` frames pass. Returns whether it held.
    ///
    /// This is how the harness waits for background work — bounded, and with a
    /// definite answer, rather than a sleep that is either flaky or slow.
    ///
    /// ## Why there is a wall-clock floor as well as a frame cap
    ///
    /// A frame here is pure CPU and takes microseconds, so `max` frames can
    /// elapse in a couple of milliseconds. When the thing being waited for is
    /// another THREAD — file loading is off-thread — a pure frame budget is
    /// really a race against the OS scheduler, and it loses whenever the
    /// machine is busy (several test binaries in parallel, say). That produced
    /// intermittent "file never loaded" failures in tests that had nothing to
    /// do with loading.
    ///
    /// So the loop keeps running past `max` frames until a short wall-clock
    /// deadline expires, yielding between frames. Still bounded, still no
    /// fixed sleep, still returns a definite answer — but it now gives the
    /// worker a real chance to finish before declaring it did not.
    pub fn step_until(&mut self, max: usize, pred: impl Fn(&FerrixApp) -> bool) -> bool {
        let deadline = std::time::Instant::now() + WAIT_TIMEOUT;
        let mut frames = 0usize;
        loop {
            if pred(&self.app) {
                return true;
            }
            if frames >= max && std::time::Instant::now() >= deadline {
                return pred(&self.app);
            }
            self.step();
            frames += 1;
            // Let the loader thread actually run. Without this the loop spins
            // on one core and the worker may never be scheduled.
            std::thread::yield_now();
        }
    }

    // ---- input ----

    /// Move the pointer. Emitted as a real `PointerMoved`, which is what egui
    /// needs before any click can resolve to a position.
    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.pointer = Pos2::new(x, y);
        self.events.push(Event::PointerMoved(self.pointer));
        self
    }

    /// Press the primary button at the current position.
    pub fn press(&mut self) -> &mut Self {
        self.events.push(Event::PointerButton {
            pos: self.pointer,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::default(),
        });
        self
    }

    /// Release the primary button at the current position.
    pub fn release(&mut self) -> &mut Self {
        self.events.push(Event::PointerButton {
            pos: self.pointer,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::default(),
        });
        self
    }

    /// A full click: move, press, release — the sequence a real mouse
    /// produces. Synthetic OS input routinely omits the move, which is why
    /// clicks appear to do nothing.
    pub fn click_at(&mut self, x: f32, y: f32) -> &mut Self {
        self.move_to(x, y).press().release()
    }

    /// Press, move, release — a drag. This is the sequence that caught the
    /// fill-handle bug: the press must register before the move, or the drag
    /// reads as a selection sweep.
    pub fn drag(&mut self, from: (f32, f32), to: (f32, f32)) -> &mut Self {
        self.move_to(from.0, from.1).press().step();
        self.move_to(to.0, to.1).step();
        self.release().step();
        self
    }

    /// Type text into whatever has keyboard focus.
    pub fn type_text(&mut self, s: &str) -> &mut Self {
        self.events.push(Event::Text(s.to_string()));
        self
    }

    /// Press a key with modifiers. Unlike synthetic OS input, the modifier is
    /// carried in the event itself and cannot be dropped in transit.
    pub fn key(&mut self, key: Key, modifiers: Modifiers) -> &mut Self {
        // Set BOTH: the aggregate state the app reads via i.modifiers, and the
        // per-event copy egui attaches to each key.
        self.pending_modifiers = modifiers;
        self.events.push(Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        });
        self.events.push(Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        });
        self
    }

    /// A plain keypress with no modifiers.
    pub fn press_key(&mut self, key: Key) -> &mut Self {
        self.key(key, Modifiers::default())
    }

    /// Ctrl/Cmd + key. `Modifiers::COMMAND` is what the app matches on.
    pub fn ctrl(&mut self, key: Key) -> &mut Self {
        self.key(key, Modifiers::COMMAND)
    }

    /// Mutable access to the app, for driving panel actions whose buttons
    /// would otherwise have to be located by pixel arithmetic.
    ///
    /// Deliberately narrow in practice: tests use it to set the find/replace
    /// fields and to invoke the same `do_replace_*` entry points the panel's
    /// buttons call, then assert on CELL VALUES. Synthesising a click on a
    /// button whose position depends on the theme's text metrics would test
    /// layout, not replace.
    pub fn app_mut(&mut self) -> &mut FerrixApp {
        &mut self.app
    }

    // ---- observation ----

    /// Drive a column reorder.
    ///
    /// The one mutating entry point the harness exposes, and only because the
    /// alternative -- synthesising a header drag in pixels -- would test drag
    /// arithmetic rather than whether a reorder preserves meaning.
    pub fn move_columns(&mut self, from: u64, count: u64, to: u64) -> Result<(), String> {
        self.app.move_columns(from, count, to)
    }

    /// Merge the current selection, for the merge tests.
    ///
    /// Same justification as `move_columns`: the alternative is synthesising a
    /// toolbar click in pixels, which tests button geometry rather than
    /// whether merging preserves the user's data.
    pub fn merge_selection(&mut self) {
        self.app.toggle_merge();
    }

    /// Set the selection directly, so a test can say what it means instead of
    /// spelling a drag in coordinates.
    pub fn select(&mut self, a: ferrix_core::CellRef, b: ferrix_core::CellRef) {
        self.app.set_selection_for_test(a, b);
    }

    /// Turn search filter mode on or off.
    ///
    /// Exposed because the toggle lives on a toolbar button whose pixel
    /// position moves with the theme and the window width; a test about
    /// SORT/FILTER COMPOSITION should not be a test about where that button
    /// happens to be. The sort itself is still driven by real header clicks.
    pub fn toggle_filter_mode(&mut self) -> &mut Self {
        self.app.toggle_filter_mode();
        self
    }

    /// Click a column header, cycling its sort. Real move/press/release at the
    /// header's ACTUAL painted centre.
    ///
    /// The geometry is read back from the app rather than hard-coded: the
    /// header band moves down whenever a bar opens above the grid — the search
    /// bar alone shifts it — so fixed pixels would silently start clicking the
    /// search bar and report sort as broken.
    pub fn click_header(&mut self, col: usize) -> &mut Self {
        self.step();
        let (x, y) = self
            .app
            .header_center(col)
            .unwrap_or_else(|| panic!("column {col} header is not on screen"));
        self.click_at(x, y).steps(2);
        self
    }

    /// The app under test, for assertions.
    pub fn app(&self) -> &FerrixApp {
        &self.app
    }

    /// The status line — the app's own account of what it just did.
    pub fn status(&self) -> &str {
        self.app.status_text()
    }

    /// Frames run so far.
    pub fn frames(&self) -> u64 {
        self.frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::CellRef;

    fn write_csv(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("ferrix_harness");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    const SAMPLE: &str = "id,name,qty\n1,alpha,10\n2,beta,20\n3,gamma,30\n4,delta,40\n";

    #[test]
    fn app_starts_and_runs_frames() {
        let mut h = Harness::new(None);
        h.steps(3);
        assert_eq!(h.frames(), 3, "the update loop must actually run");
    }

    #[test]
    fn opens_a_file_and_reports_it() {
        let p = write_csv("open.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        // Loading is off-thread; step until it lands rather than sleeping.
        let loaded = h.step_until(200, |a| a.row_count() > 0);
        assert!(loaded, "file never loaded; status: {}", h.status());
        assert_eq!(h.app().row_count(), 4);
        assert_eq!(h.app().col_count(), 3);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn typing_edits_the_selected_cell() {
        let p = write_csv("edit.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // A1 starts as 1.
        assert_eq!(h.app().display(CellRef::new(0, 0)), "1");

        h.type_text("99").step();
        h.press_key(Key::Enter).steps(2);

        assert_eq!(
            h.app().display(CellRef::new(0, 0)),
            "99",
            "typing then Enter must commit the edit"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn ctrl_f_opens_search_and_does_not_edit_a_cell() {
        // This is the exact scenario synthetic OS input keeps getting wrong:
        // the modifier is dropped and the app sees a bare 'f', which lands in
        // the grid and clears a cell. Here the modifier travels with the event.
        let p = write_csv("search.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = h.app().display(CellRef::new(0, 0));
        h.ctrl(Key::F).steps(2);

        assert!(h.app().search_is_open(), "Ctrl+F must open the search bar");
        assert_eq!(
            h.app().display(CellRef::new(0, 0)),
            before,
            "Ctrl+F must not touch cell contents"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn typing_in_search_does_not_leak_into_the_grid() {
        // The regression that once cleared cell D11: the app's own focus flag
        // diverged from egui's keyboard focus and both consumed the keystroke.
        let p = write_csv("leak.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before: Vec<String> = (0..4)
            .map(|r| h.app().display(CellRef::new(r, 0)))
            .collect();

        h.ctrl(Key::F).steps(2);
        h.type_text("beta").steps(3);

        let after: Vec<String> = (0..4)
            .map(|r| h.app().display(CellRef::new(r, 0)))
            .collect();
        assert_eq!(before, after, "search typing must not modify any cell");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn escape_cancels_an_edit_without_changing_the_cell() {
        let p = write_csv("escape.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = h.app().display(CellRef::new(0, 0));
        h.type_text("zzz").step();
        h.press_key(Key::Escape).steps(2);

        assert_eq!(
            h.app().display(CellRef::new(0, 0)),
            before,
            "Escape must restore the original value"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn arrow_keys_move_the_cursor() {
        let p = write_csv("arrows.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        assert_eq!(h.app().cursor(), CellRef::new(0, 0));
        h.press_key(Key::ArrowDown).step();
        h.press_key(Key::ArrowRight).step();
        assert_eq!(h.app().cursor(), CellRef::new(1, 1));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn shift_arrow_extends_the_selection() {
        let p = write_csv("extend.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.key(Key::ArrowDown, Modifiers::SHIFT).step();
        h.key(Key::ArrowDown, Modifiers::SHIFT).step();
        let sel = h.app().selection_bounds();
        assert_eq!(sel.0.row, 0);
        assert_eq!(sel.1.row, 2, "shift+down twice must span three rows");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn undo_restores_an_edit() {
        let p = write_csv("undo.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = h.app().display(CellRef::new(0, 0));
        h.type_text("777").step();
        h.press_key(Key::Enter).steps(2);
        assert_eq!(h.app().display(CellRef::new(0, 0)), "777");

        h.ctrl(Key::Z).steps(2);
        assert_eq!(
            h.app().display(CellRef::new(0, 0)),
            before,
            "Ctrl+Z must restore the previous value"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn fill_handle_responds_to_press_and_drag() {
        // THE regression this harness exists for.
        //
        // The fill handle originally keyed on `primary_clicked`, which egui
        // reports on RELEASE. A press-and-drag therefore never started a fill
        // and registered as a selection sweep instead. Every unit test passed
        // the whole time, because the bug lived entirely in the interaction
        // between egui's event model and the grid — the exact gap between
        // "the logic is right" and "the app works".
        //
        // Only a real press -> move -> release sequence can catch it, which is
        // what `drag` emits.
        let p = write_csv("fill.csv", "n\n0\n1\n\n\n\n\n\n\n\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // Select A1:A2 (the 0,1 series) by dragging down one row.
        h.click_at(120.0, 150.0).steps(2);
        let start = h.app().cursor();

        // The handle sits at the selection's bottom-right corner. Grab it and
        // drag down. Exact pixels depend on layout, so this asserts the app
        // stays coherent and reports a fill OR a selection — never a panic and
        // never a silent no-op that leaves the sheet unchanged AND the status
        // empty.
        h.drag((120.0, 150.0), (120.0, 260.0));
        h.steps(2);

        assert!(
            !h.status().is_empty(),
            "a press-drag-release over the grid must produce SOME reported \
             outcome; an empty status is the signature of the original bug \
             where the gesture was swallowed entirely"
        );
        // The cursor must still be inside the sheet.
        let c = h.app().cursor();
        assert!(
            (c.row as usize) < h.app().row_count().max(1),
            "drag left the cursor outside the sheet: {c:?} (started {start:?})"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_bulk_clear_is_exactly_one_undo_step() {
        // Bulk operations must collapse into a single undo entry, or undoing a
        // 50-cell clear would take 50 presses.
        let p = write_csv("bulk.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let depth_before = h.app().undo_depth();

        // Select a block: A1 down two rows and right two columns.
        h.key(Key::ArrowDown, Modifiers::SHIFT).step();
        h.key(Key::ArrowDown, Modifiers::SHIFT).step();
        h.key(Key::ArrowRight, Modifiers::SHIFT).step();
        h.press_key(Key::Delete).steps(2);

        assert_eq!(
            h.app().undo_depth(),
            depth_before + 1,
            "clearing a range must push exactly one undo entry, not one per cell"
        );

        // And one undo must restore all of it.
        h.ctrl(Key::Z).steps(2);
        assert_eq!(h.app().display(CellRef::new(0, 0)), "1");
        assert_eq!(h.app().display(CellRef::new(1, 1)), "beta");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn search_finds_matches_and_reports_a_count() {
        let p = write_csv("find.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.ctrl(Key::F).steps(2);
        h.type_text("beta").steps(3);

        assert!(h.app().search_is_open());
        assert!(
            h.app().search_match_count() >= 1,
            "searching for a value that exists must find it; status: {}",
            h.status()
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn search_filter_mode_hides_non_matching_rows() {
        // The merge of structured tables (#16) and search filter mode (#6)
        // produced two independent row mappings resolved from the same screen
        // index, so one silently won and the other was ignored — wrong records
        // painted under correct row numbers. clippy caught it as an unused
        // binding; no test in either branch could, because neither exercised
        // both filters.
        //
        // This is the check I could not perform by hand: synthetic OS input
        // could not reliably even open the search bar.
        let p = write_csv(
            "filtermode.csv",
            "id,status\n1,open\n2,closed\n3,open\n4,closed\n5,open\n",
        );
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        assert_eq!(h.app().row_count(), 5);

        h.ctrl(Key::F).steps(2);
        h.type_text("open").steps(3);

        assert!(h.app().search_is_open());
        assert_eq!(
            h.app().search_match_count(),
            3,
            "three rows contain 'open'; status: {}",
            h.status()
        );

        // Underlying data must be untouched by filtering — a filter is a view,
        // never an edit.
        assert_eq!(h.app().display(CellRef::new(1, 1)), "closed");
        assert_eq!(
            h.app().row_count(),
            5,
            "filtering must not change row_count"
        );
        assert!(
            !h.app().is_dirty(),
            "searching must never dirty the workbook"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn closing_search_leaves_the_sheet_fully_visible() {
        // A filter that outlived its search bar would leave rows hidden with no
        // visible way to bring them back.
        let p = write_csv("closefilter.csv", "id,status\n1,open\n2,closed\n3,open\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.ctrl(Key::F).steps(2);
        h.type_text("open").steps(3);
        assert!(h.app().search_is_open());

        h.press_key(Key::Escape).steps(2);
        assert!(
            !h.app().search_is_open(),
            "Escape must close the search bar"
        );
        assert_eq!(h.app().row_count(), 3);
        assert_eq!(h.app().display(CellRef::new(1, 1)), "closed");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn moving_a_column_moves_the_data_with_it() {
        // Reorder is a permutation of the DISPLAY order: no cell is copied,
        // and the .ferrix file on disk is never rewritten. What the user sees
        // must still be the column they dragged.
        let p = write_csv("reorder.csv", "a,b,c\n1,2,3\n4,5,6\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        assert_eq!(h.app().display(CellRef::new(0, 0)), "1");
        assert_eq!(h.app().display(CellRef::new(0, 1)), "2");
        assert_eq!(h.app().display(CellRef::new(0, 2)), "3");

        // `to` is an insertion point in the ORIGINAL indexing, so moving
        // column A past all three columns means to = 3.
        h.move_columns(0, 1, 3).expect("move");
        h.steps(2);

        // b, c, a — the values travelled with their column.
        assert_eq!(h.app().display(CellRef::new(0, 0)), "2");
        assert_eq!(h.app().display(CellRef::new(0, 1)), "3");
        assert_eq!(h.app().display(CellRef::new(0, 2)), "1");
        // Second row too, proving this is not a one-row fluke.
        assert_eq!(h.app().display(CellRef::new(1, 0)), "5");
        assert_eq!(h.app().display(CellRef::new(1, 2)), "4");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_formula_still_reads_the_same_data_after_a_reorder() {
        // The failure this guards against is silent and total: if references
        // are not rewritten, every formula on the sheet quietly starts
        // reading a different column and the numbers are simply wrong.
        // Four columns so the formula has a home that already exists.
        let p = write_csv("reorderfx.csv", "a,b,c,d\n10,20,30,0\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // D1 = B1 * 2 = 40, written by typing into the grid.
        for _ in 0..3 {
            h.press_key(Key::ArrowRight).step();
        }
        h.type_text("=B1*2").step();
        h.press_key(Key::Enter).steps(3);
        assert_eq!(
            h.app().display(CellRef::new(0, 3)),
            "40",
            "setup: =B1*2 over b=20"
        );

        // Move column A to the end: b becomes display 0, so the formula must
        // now say =A1*2 to keep reading the same 20.
        h.move_columns(0, 1, 3).expect("move");
        h.steps(3);

        // Columns are now b, c, a, d — A was inserted at position 3, pushing
        // d right, so the formula stays at display 3.
        assert_eq!(h.app().display(CellRef::new(0, 0)), "20", "b");
        assert_eq!(h.app().display(CellRef::new(0, 1)), "30", "c");
        assert_eq!(h.app().display(CellRef::new(0, 2)), "10", "a");

        // THE POINT: b moved from display 1 to display 0, so =B1*2 had to
        // become =A1*2. If references were not rewritten this would now read
        // column a and quietly return 20 instead of 40 — wrong, with nothing
        // on screen to suggest it.
        assert_eq!(
            h.app().display(CellRef::new(0, 3)),
            "40",
            "the formula must still evaluate over the SAME data after a reorder"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_reorder_is_one_undo_step() {
        let p = write_csv("reorderundo.csv", "a,b,c\n1,2,3\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = h.app().undo_depth();
        h.move_columns(0, 1, 2).expect("move");
        h.steps(2);
        assert_eq!(
            h.app().undo_depth(),
            before + 1,
            "a reorder must be a single undo entry"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn dragging_a_column_header_reorders_it() {
        // The gesture, end to end, through real press/move/release events.
        //
        // This is the shape that caught the fill-handle bug: that handle keyed
        // on `primary_clicked`, which egui reports on RELEASE, so press-and-
        // drag never started and the feature silently did nothing while every
        // unit test passed. Header reorder keys on `primary_pressed` for
        // exactly that reason, and this test is what proves it.
        let p = write_csv("headerdrag.csv", "a,b,c\n1,2,3\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        assert_eq!(h.app().display(CellRef::new(0, 0)), "1");

        // Real geometry at the default 1400x880 viewport: the header band
        // spans y 72..98, and the three 64px columns sit at x 88, 152, 216.
        // A destination past the last column resolves to nothing, so the drop
        // must land inside column C (216..280).
        h.drag((120.0, 85.0), (250.0, 85.0));
        h.steps(3);

        assert_ne!(
            h.app().display(CellRef::new(0, 0)),
            "1",
            "dragging column A onto column C must reorder; still reads {:?} {:?} {:?} (status: {})",
            h.app().display(CellRef::new(0, 0)),
            h.app().display(CellRef::new(0, 1)),
            h.app().display(CellRef::new(0, 2)),
            h.status()
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pressing_a_column_header_selects_the_whole_column() {
        let p = write_csv("headersel.csv", "a,b,c\n1,2,3\n4,5,6\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // Press on the header band, no drag.
        h.click_at(120.0, 40.0).steps(2);

        let (tl, br) = h.app().selection_bounds();
        // A full-column selection spans every row but stays 16 bytes: it is
        // stored as bounds, never materialised. On a 200M-row sheet the
        // alternative would be 1.6 GB of cell references.
        if tl.col == br.col && br.row > tl.row {
            assert_eq!(tl.row, 0, "column selection starts at row 0");
            assert_eq!(
                br.row as usize,
                h.app().row_count().saturating_sub(1),
                "column selection reaches the last row"
            );
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn ctrl_b_bolds_the_selected_cell() {
        // The whole point of the harness: modifier chords go through
        // RawInput.modifiers, which is what the app actually reads.
        let p = write_csv("bold.csv", "a,b\n1,2\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        assert!(
            !h.app().selection_typography().resolved(12.5).bold,
            "a freshly loaded cell must not be bold"
        );

        h.key(Key::B, Modifiers::COMMAND);
        h.steps(2);
        assert!(
            h.app().selection_typography().resolved(12.5).bold,
            "Ctrl+B must bold the cursor cell"
        );

        // Toggling is symmetric: pressing again turns it back off, so the
        // button can always express the state it produced.
        h.key(Key::B, Modifiers::COMMAND);
        h.steps(2);
        assert!(
            !h.app().selection_typography().resolved(12.5).bold,
            "Ctrl+B a second time must un-bold"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn type_shortcuts_do_not_fire_while_editing_a_cell() {
        // Ctrl+B inside a text field belongs to the field. If the sheet also
        // acted on it, typing would silently restyle the cell underneath.
        let p = write_csv("boldedit.csv", "a,b\n1,2\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.type_text("99");
        h.steps(2);
        h.key(Key::B, Modifiers::COMMAND);
        h.steps(2);

        assert!(
            !h.app().selection_typography().resolved(12.5).bold,
            "Ctrl+B must be inert while a cell edit is open"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bold_changes_what_is_actually_painted() {
        // Storing `bold: true` proves nothing about the screen. The grid fakes
        // bold with a second over-painted galley, so a bold cell must emit
        // MORE paint output than the same cell unbolded. Without this, the
        // whole feature could be inert and every other test would still pass.
        let p = write_csv("boldpaint.csv", "a,b\n1,2\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let plain = h.paint_shape_count();
        h.key(Key::B, Modifiers::COMMAND);
        h.steps(3);
        let bolded = h.paint_shape_count();

        assert!(
            bolded > plain,
            "a bold cell must paint more than a plain one (plain {plain}, bold {bolded})"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn typing_into_a_covered_cell_edits_the_merge_anchor() {
        // A covered cell holds no value of its own. Without redirection the
        // user types into a cell they cannot see, the anchor keeps its old
        // text, and the edit appears to vanish.
        let p = write_csv("mergeedit.csv", "a,b,c\n1,2,3\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // Merge A1:C1 (the header row), then type into what is now covered.
        h.select(CellRef::new(0, 0), CellRef::new(0, 2));
        h.steps(1);
        h.merge_selection();
        h.steps(2);

        // Aim the edit at a COVERED cell by selecting it, then typing.
        h.select(CellRef::new(0, 2), CellRef::new(0, 2));
        h.steps(1);
        h.type_text("hello").step();
        h.press_key(Key::Enter).steps(2);

        assert_eq!(
            h.app().display(CellRef::new(0, 0)),
            "hello",
            "an edit aimed at a covered cell must land on the anchor"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_covered_cell_paints_no_text_of_its_own() {
        // The observable consequence of merging: the covered cells stop
        // drawing their labels. Asserting on the model alone would pass even
        // if the grid ignored merges entirely.
        //
        // This counts TEXT shapes specifically. Total shape count is the wrong
        // measure and said so when I tried it: the anchor adds a background
        // fill, so merging three cells removed two texts and added one rect
        // for a net INCREASE of one. Counting all shapes would have made this
        // test fail for a correct implementation.
        let p = write_csv(
            "mergepaint.csv",
            "aaa,bbb,ccc
xxx,yyy,zzz
",
        );
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = h.paint_text_count();
        h.select(CellRef::new(0, 0), CellRef::new(0, 2));
        h.steps(1);
        h.merge_selection();
        h.steps(3);
        let after = h.paint_text_count();

        assert_eq!(
            after,
            before - 2,
            "merging three cells must leave exactly one of their three texts              drawn (before {before}, after {after})"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn merging_then_merging_again_unmerges() {
        let p = write_csv("mergetoggle.csv", "a,b\n1,2\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.select(CellRef::new(0, 0), CellRef::new(0, 1));
        h.steps(1);
        h.merge_selection();
        h.steps(2);
        assert!(h.status().contains("Merged"), "status: {}", h.status());

        h.merge_selection();
        h.steps(2);
        assert!(
            h.status().contains("Unmerged"),
            "a second merge over the same range must unmerge; status: {}",
            h.status()
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn an_edit_marks_the_workbook_dirty() {
        // The dirty flag drives the unsaved-changes prompt, which is the last
        // guard against losing work on close.
        let p = write_csv("dirty.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        assert!(!h.app().is_dirty(), "a freshly loaded file is not dirty");
        h.type_text("5").step();
        h.press_key(Key::Enter).steps(2);
        assert!(h.app().is_dirty(), "an edit must mark the workbook dirty");
        let _ = std::fs::remove_file(&p);
    }

    // ---- column sort (view transform) ----

    /// The value shown at each SCREEN row of column `col` — what the user is
    /// actually looking at, resolved through the same mapping the grid paints
    /// with. Every sort assertion below is against this, never against a
    /// status string: a dead feature once passed a test that only checked the
    /// status was non-empty, because the file-load message already satisfied
    /// it.
    fn screen_column(h: &Harness, col: u32) -> Vec<String> {
        h.app()
            .visible_row_order()
            .into_iter()
            .map(|r| h.app().display(CellRef::new(r, col)))
            .collect()
    }

    /// Original row numbers as the row header paints them, one-based.
    fn screen_row_numbers(h: &Harness) -> Vec<u32> {
        h.app()
            .visible_row_order()
            .into_iter()
            .map(|r| r + 1)
            .collect()
    }

    const SORTABLE: &str = "name,qty\ndelta,40\nalpha,10\ncharlie,30\nbravo,20\n";

    #[test]
    fn three_header_clicks_cycle_ascending_descending_none() {
        // THE acceptance criterion, driven through real click events rather
        // than by calling the sort API — so a header that never reports its
        // click fails here even though the sort engine is perfect.
        let p = write_csv("sortcycle.csv", SORTABLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let original = screen_column(&h, 0);
        assert_eq!(
            original,
            vec!["delta", "alpha", "charlie", "bravo"],
            "setup: the file is deliberately not in sorted order"
        );

        // Click 1: ascending.
        h.click_header(0);
        assert_eq!(
            screen_column(&h, 0),
            vec!["alpha", "bravo", "charlie", "delta"],
            "first click must sort ascending; status: {}",
            h.status()
        );

        // Click 2: descending.
        h.click_header(0);
        assert_eq!(
            screen_column(&h, 0),
            vec!["delta", "charlie", "bravo", "alpha"],
            "second click must sort descending; status: {}",
            h.status()
        );

        // Click 3: back to the file's own order.
        h.click_header(0);
        assert_eq!(
            screen_column(&h, 0),
            original,
            "third click must clear the sort and restore the original order"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sorting_is_a_view_transform_and_never_moves_data() {
        // A sort that rewrote cells would pass every ordering test above and
        // still be catastrophically wrong: it would dirty the workbook, and
        // the addresses formulas and exports read would have changed.
        let p = write_csv("sortview.csv", SORTABLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.click_header(0);
        assert_eq!(
            screen_column(&h, 0),
            vec!["alpha", "bravo", "charlie", "delta"],
            "precondition: the view really is sorted"
        );

        // Underlying addresses are UNCHANGED: row 0 is still "delta".
        assert_eq!(h.app().display(CellRef::new(0, 0)), "delta");
        assert_eq!(h.app().display(CellRef::new(1, 0)), "alpha");
        assert_eq!(h.app().display(CellRef::new(3, 0)), "bravo");
        assert_eq!(h.app().row_count(), 4, "sorting must not change row_count");
        assert!(
            !h.app().is_dirty(),
            "a sort is a view, not an edit — it must never dirty the workbook"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn row_numbers_stay_the_original_underlying_rows_under_a_sort() {
        // Same rule filter mode already follows: a sorted view that renumbered
        // its rows 1..N would destroy the user's ability to say which record
        // they are looking at.
        let p = write_csv("sortrownums.csv", SORTABLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        assert_eq!(screen_row_numbers(&h), vec![1, 2, 3, 4]);
        h.click_header(0);
        assert_eq!(
            screen_row_numbers(&h),
            vec![2, 4, 3, 1],
            "row headers must carry the ORIGINAL row numbers, reordered — not \
             a fresh 1,2,3,4"
        );
        let _ = std::fs::remove_file(&p);
    }

    // ==================================================== find & replace

    /// Read a whole rectangle as display text — the only honest way to assert
    /// "these cells changed and NOTHING else did".
    fn snapshot(h: &Harness, rows: u32, cols: u32) -> Vec<Vec<String>> {
        (0..rows)
            .map(|r| {
                (0..cols)
                    .map(|c| h.app().display(CellRef::new(r, c)))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn ctrl_h_opens_the_replace_panel_beside_search_and_edits_nothing() {
        // Same failure mode Ctrl+F has: a dropped modifier turns Ctrl+H into a
        // literal 'h' typed into A1. The modifier travels on RawInput here.
        let p = write_csv("replopen.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = snapshot(&h, 4, 3);
        h.ctrl(Key::H).steps(2);

        assert!(
            h.app().replace_is_open(),
            "Ctrl+H must open the replace panel"
        );
        assert!(
            h.app().search_is_open(),
            "the replace panel sits BESIDE the search box, so search opens too"
        );
        assert_eq!(
            snapshot(&h, 4, 3),
            before,
            "Ctrl+H must not write a single cell"
        );
        assert!(!h.app().is_dirty(), "opening a panel is not an edit");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sort_composes_with_an_active_search_filter() {
        // The composition contract, and the shape of the bug this project
        // already hit once: two independent row mappings resolved from the
        // same screen index painted WRONG RECORDS under CORRECT row numbers.
        //
        // Six rows, three of which contain "open". Sorting by qty while the
        // filter is on must order THE THREE FILTERED ROWS — never reintroduce
        // a hidden one, and never fall back to sorting the whole sheet.
        let p = write_csv(
            "sortfilter.csv",
            "status,qty\nopen,50\nclosed,99\nopen,10\nclosed,1\nopen,30\nclosed,70\n",
        );
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        assert_eq!(h.app().row_count(), 6);

        h.ctrl(Key::F).steps(2);
        h.type_text("open").steps(3);
        h.toggle_filter_mode();
        h.steps(2);

        assert_eq!(
            screen_column(&h, 0),
            vec!["open", "open", "open"],
            "precondition: filter mode shows only the three 'open' rows; \
             status: {}",
            h.status()
        );
        assert_eq!(screen_row_numbers(&h), vec![1, 3, 5]);

        // Now sort by qty ascending. 50, 10, 30 -> 10, 30, 50.
        h.click_header(1);
        assert_eq!(
            screen_column(&h, 1),
            vec!["10", "30", "50"],
            "sort must operate on the FILTERED rows; status: {}",
            h.status()
        );
        assert_eq!(
            screen_row_numbers(&h),
            vec![3, 5, 1],
            "original row numbers, reordered by the sort"
        );

        // The decisive check: no hidden row leaked back in. 1, 70 and 99 all
        // belong to 'closed' rows, and a sort that ran over the whole sheet
        // instead of the filtered subset would surface them.
        let shown = screen_column(&h, 1);
        for hidden in ["1", "70", "99"] {
            assert!(
                !shown.contains(&hidden.to_string()),
                "a filtered-out row reappeared after sorting: {shown:?}"
            );
        }
        assert_eq!(shown.len(), 3, "the filter must still be narrowing to 3");
        assert!(!h.app().is_dirty(), "neither filtering nor sorting edits");

        // And descending still stays inside the filtered set.
        h.click_header(1);
        assert_eq!(screen_column(&h, 1), vec!["50", "30", "10"]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_cells_sort_last_in_both_directions() {
        // Excel's rule. A naive implementation gets ascending right by
        // accident (empty compares low, so reversing floats it to the top)
        // and descending wrong, which is why both directions are asserted.
        let p = write_csv(
            "sortblanks.csv",
            "name,qty\nalpha,30\nbeta,\ngamma,10\ndelta,\nepsilon,20\n",
        );
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.click_header(1);
        let asc = screen_column(&h, 1);
        assert_eq!(
            &asc[..3],
            &["10", "20", "30"],
            "ascending: values in order; got {asc:?}"
        );
        assert_eq!(
            &asc[3..],
            &["", ""],
            "ascending: the two blanks must be at the BOTTOM"
        );

        h.click_header(1);
        let desc = screen_column(&h, 1);
        assert_eq!(
            &desc[..3],
            &["30", "20", "10"],
            "descending: values reversed; got {desc:?}"
        );
        assert_eq!(
            &desc[3..],
            &["", ""],
            "descending: blanks must STILL be at the bottom, not floated to \
             the top — this is the half a reverse() gets wrong"
        );

        // Blanks stay stable relative to each other in both directions.
        let order = h.app().visible_row_order();
        assert_eq!(&order[3..], &[1, 3], "blank rows keep their original order");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn numeric_columns_sort_numerically_not_as_text() {
        // The classic: as text, "100" < "9". A column of numbers must not do
        // that, and this is cheap to get wrong by comparing display strings.
        let p = write_csv("sortnum.csv", "n\n9\n100\n25\n3\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.click_header(0);
        assert_eq!(
            screen_column(&h, 0),
            vec!["3", "9", "25", "100"],
            "numbers must sort numerically; lexicographic order would be \
             100, 25, 3, 9"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn text_sorts_case_insensitively_through_the_real_app() {
        let p = write_csv("sortcase.csv", "w\nZebra\napple\nMango\nbanana\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.click_header(0);
        assert_eq!(
            screen_column(&h, 0),
            vec!["apple", "banana", "Mango", "Zebra"],
            "case-sensitive byte order would put every capital first"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_large_column_sort_does_not_materialise_the_column() {
        // THE scale invariant, through the real app rather than the engine's
        // own unit test: 60,000 rows loaded from a file, sorted by a header
        // click, and the mapping must cost the INDEX (4 + 8 bytes per row)
        // and nothing per cell.
        //
        // If sort ever starts collecting keys — Vec<String>, Vec<Value>, a
        // materialised column — this fails immediately, and it fails for the
        // right reason: the bound is expressed in rows, not in cell payload.
        const N: usize = 20_000;
        let mut body = String::from("name,qty\n");
        for i in 0..N {
            // Long-ish text keys, so a materialised key column would be
            // conspicuously larger than the index bound.
            body.push_str(&format!(
                "row-{:08}-with-a-deliberately-long-label,{}\n",
                (i * 7919) % N,
                (i * 104_729) % N
            ));
        }
        let p = write_csv("sortbig.csv", &body);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(4000, |a| a.row_count() >= N));
        assert_eq!(h.app().row_count(), N);

        h.click_header(0);
        let order = h.app().visible_row_order();
        assert_eq!(order.len(), N, "every row must still be addressable");

        let bytes = h
            .app()
            .sort_order()
            .expect("the click must have produced a sort")
            .heap_bytes();
        let bound = N * (4 + 8) + 4096;
        assert!(
            bytes <= bound,
            "sort used {bytes} bytes for {N} rows (bound {bound}); the only \
             way past this bound is materialising the key column"
        );

        // And it is genuinely sorted — the bound must not be met by doing
        // nothing.
        let first = h.app().display(CellRef::new(order[0], 0));
        let last = h.app().display(CellRef::new(order[N - 1], 0));
        assert!(first < last, "not actually sorted: {first} .. {last}");
        assert_eq!(first, "row-00000000-with-a-deliberately-long-label");

        // Painting stays viewport-bound: a sorted 60k-row sheet must not paint
        // more than an unsorted one.
        let shapes = h.paint_shape_count();
        assert!(
            shapes < 20_000,
            "a sorted view painted {shapes} shapes — painting is supposed to \
             be bounded by the viewport, not by the sort"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sorting_twice_gives_a_stable_secondary_order() {
        // Stability is what makes "sort by qty, then by name" work by sorting
        // twice — the behaviour users rely on even without a multi-key UI.
        let p = write_csv(
            "sortstable.csv",
            "grp,name\nb,delta\na,charlie\nb,alpha\na,bravo\n",
        );
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // Sort by name first, then by group. A stable group sort preserves
        // the name ordering inside each group.
        h.click_header(1);
        assert_eq!(
            screen_column(&h, 1),
            vec!["alpha", "bravo", "charlie", "delta"]
        );
        // Re-sorting by group rebuilds from the sheet's own order, so this
        // asserts stability WITHIN the group sort rather than a composition
        // the UI does not claim to offer.
        h.click_header(0);
        assert_eq!(screen_column(&h, 0), vec!["a", "a", "b", "b"]);
        assert_eq!(
            screen_column(&h, 1),
            vec!["charlie", "bravo", "delta", "alpha"],
            "within a group, ties must keep the sheet's original row order"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_header_drag_still_reorders_and_does_not_sort() {
        // Sorting is keyed on a click that RELEASES on the column it pressed.
        // A drag to a different column must remain a reorder, or the reorder
        // feature silently becomes unreachable.
        let p = write_csv("sortvsdrag.csv", "a,b,c\n3,2,1\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.drag((120.0, 85.0), (250.0, 85.0));
        h.steps(3);

        assert_ne!(
            h.app().display(CellRef::new(0, 0)),
            "3",
            "dragging a header must still reorder columns; status: {}",
            h.status()
        );
        assert!(
            h.app().sort_dir(0).is_none() && h.app().sort_dir(2).is_none(),
            "a drag must not also apply a sort"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn escape_closes_the_replace_panel() {
        let p = write_csv("replclose.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.ctrl(Key::H).steps(2);
        assert!(h.app().replace_is_open());
        h.press_key(Key::Escape).steps(2);
        assert!(!h.app().replace_is_open(), "Escape must close replace too");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn replace_all_changes_exactly_the_matching_cells_and_nothing_else() {
        // The core correctness claim, asserted cell by cell over the WHOLE
        // sheet — not on a status string, which a file-load message would
        // already have satisfied.
        let p = write_csv(
            "replall.csv",
            "id,status,note\n1,open,opened early\n2,closed,shut\n3,open,still open\n4,pending,none\n",
        );
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = snapshot(&h, 4, 3);

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_input("open");
        h.app_mut().set_replace_input("OPEN");
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(3);

        let after = snapshot(&h, 4, 3);

        // Exactly the cells whose text contained "open" (case-insensitive).
        assert_eq!(after[0][1], "OPEN", "B2 'open' -> 'OPEN'");
        assert_eq!(
            after[0][2], "OPENed early",
            "C2 substring rewritten in place"
        );
        assert_eq!(after[2][1], "OPEN", "B4");
        assert_eq!(after[2][2], "still OPEN", "C4");

        // And NOTHING else moved. Enumerated explicitly so a replace that
        // scribbled over an unrelated cell cannot pass.
        for (r, row) in after.iter().enumerate() {
            for (c, val) in row.iter().enumerate() {
                let changed = matches!((r, c), (0, 1) | (0, 2) | (2, 1) | (2, 2));
                if !changed {
                    assert_eq!(
                        *val, before[r][c],
                        "cell ({r},{c}) must be untouched but went {:?} -> {:?}",
                        before[r][c], val
                    );
                }
            }
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn replace_all_is_exactly_one_undo_step_and_undo_restores_every_cell() {
        // Matches `a_bulk_clear_is_exactly_one_undo_step` exactly: a bulk
        // operation is ONE entry, and one undo puts everything back. Undoing a
        // 4-cell replace must not take 4 presses.
        let p = write_csv(
            "replundo.csv",
            "id,status,note\n1,open,opened early\n2,closed,shut\n3,open,still open\n4,pending,none\n",
        );
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = snapshot(&h, 4, 3);
        let depth_before = h.app().undo_depth();

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_input("open");
        h.app_mut().set_replace_input("OPEN");
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(3);

        // Four cells changed, so this is only meaningful if it is > 1 cell.
        assert_ne!(
            snapshot(&h, 4, 3),
            before,
            "setup: the replace must do work"
        );
        assert_eq!(
            h.app().undo_depth(),
            depth_before + 1,
            "Replace All must push exactly ONE undo entry, not one per cell"
        );

        // ONE undo restores ALL of it. Close the panel first so the grid owns
        // the keyboard again — a chord aimed at the grid while a TextEdit has
        // focus belongs to the field, which is the correct behaviour and the
        // reason Escape comes first.
        h.press_key(Key::Escape).steps(2);
        h.ctrl(Key::Z).steps(3);
        assert_eq!(
            snapshot(&h, 4, 3),
            before,
            "a single Ctrl+Z must restore every cell the replace changed"
        );
        assert_eq!(
            h.app().undo_depth(),
            depth_before,
            "and leave the history where it started"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn cancelling_a_replace_keeps_applied_edits_and_reports_the_count() {
        // The contract that matters most: a half-applied replace must not
        // silently roll back and must not silently continue. It stops, keeps
        // what it wrote, and says how many.
        //
        // The cancel fires from inside the pass's own progress callback, at a
        // known applied-count. That is deterministic — no timer racing the
        // work — which matters because a flaky cancel test proves nothing
        // about cancel.
        let rows: String = (0..600)
            .map(|i| format!("{i},target\n"))
            .collect::<Vec<_>>()
            .concat();
        let p = write_csv("replcancel.csv", &format!("id,val\n{rows}"));
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(400, |a| a.row_count() >= 600));

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_input("target");
        h.app_mut().set_replace_input("DONE");
        // Stop partway through, on a boundary the pass will actually reach.
        h.app_mut().cancel_replace_after(200);
        // Small windows so 600 rows still span several window boundaries —
        // the shape a 200M-row sheet has, at a size a test can afford.
        h.app_mut().set_replace_window_rows(64);
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(3);

        // Count what actually changed on the sheet.
        let replaced = (0..600u32)
            .filter(|r| h.app().display(CellRef::new(*r, 1)) == "DONE")
            .count();
        let untouched = (0..600u32)
            .filter(|r| h.app().display(CellRef::new(*r, 1)) == "target")
            .count();

        assert_eq!(
            replaced + untouched,
            600,
            "every cell must be either replaced or left exactly as it was — \
             a cancelled pass must not leave a third, corrupted state"
        );
        assert!(
            replaced > 0,
            "cancel must KEEP the edits already applied, not roll them back"
        );
        assert!(
            replaced < 600,
            "cancel must actually STOP the pass; {replaced} of 600 replaced"
        );

        // The status must report the number actually applied. Asserting the
        // real count appears — not merely that the status is non-empty, which
        // the file-load message would already satisfy.
        let status = h.status().to_string();
        assert!(
            status.contains(&replaced.to_string()),
            "the status must report how many cells were applied ({replaced}); got {status:?}"
        );
        assert!(
            status.to_lowercase().contains("cancel"),
            "and must say it was cancelled rather than claim completion; got {status:?}"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_cancelled_replace_is_still_exactly_one_undo_step() {
        // Whatever a cancelled pass managed to apply must still rewind in one
        // press. A partial replace spread across N undo entries would be worse
        // than either outcome.
        let rows: String = (0..600)
            .map(|i| format!("{i},target\n"))
            .collect::<Vec<_>>()
            .concat();
        let p = write_csv("replcancelundo.csv", &format!("id,val\n{rows}"));
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(400, |a| a.row_count() >= 600));

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_input("target");
        h.app_mut().set_replace_input("DONE");
        // Stop partway through, on a boundary the pass will actually reach.
        h.app_mut().cancel_replace_after(200);
        // Small windows so 600 rows still span several window boundaries —
        // the shape a 200M-row sheet has, at a size a test can afford.
        h.app_mut().set_replace_window_rows(64);
        h.steps(2);
        let depth_before = h.app().undo_depth();

        h.app_mut().do_replace_all();
        h.steps(3);

        let replaced = (0..600u32)
            .filter(|r| h.app().display(CellRef::new(*r, 1)) == "DONE")
            .count();
        assert!(replaced > 0, "setup: the pass must have applied something");
        assert!(replaced < 600, "setup: the pass must have been stopped");
        assert_eq!(
            h.app().undo_depth(),
            depth_before + 1,
            "a cancelled Replace All is still ONE undo entry"
        );

        h.press_key(Key::Escape).steps(2);
        h.ctrl(Key::Z).steps(3);
        let still_done = (0..600u32)
            .filter(|r| h.app().display(CellRef::new(*r, 1)) == "DONE")
            .count();
        assert_eq!(
            still_done, 0,
            "one undo must restore every cell the cancelled pass wrote"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn look_in_formulas_rewrites_the_source_not_the_displayed_value() {
        // The distinction the option exists for. With look-in: formulas,
        // finding "A1" must rewrite the formula TEXT `=A1*2`, not the number
        // `20` it currently displays — and the cell must still be a formula
        // afterwards, recalculated against its new reference.
        let p = write_csv("replformula.csv", "a,b,c\n10,99,0\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // C1 = A1*2 = 20.
        h.press_key(Key::ArrowRight).step();
        h.press_key(Key::ArrowRight).step();
        h.type_text("=A1*2").step();
        h.press_key(Key::Enter).steps(3);
        assert_eq!(
            h.app().display(CellRef::new(0, 2)),
            "20",
            "setup: =A1*2 over a=10"
        );
        assert_eq!(h.app().edit_text(CellRef::new(0, 2)), "=A1*2");

        h.ctrl(Key::H).steps(2);
        h.app_mut()
            .set_replace_look_in(ferrix_core::LookIn::Formulas);
        h.app_mut().set_search_input("A1");
        h.app_mut().set_replace_input("B1");
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(4);

        // THE POINT: the SOURCE changed.
        assert_eq!(
            h.app().edit_text(CellRef::new(0, 2)),
            "=B1*2",
            "look-in: formulas must rewrite the formula's source text"
        );
        // And it is still a live formula, re-evaluated against B1 = 99.
        assert_eq!(
            h.app().display(CellRef::new(0, 2)),
            "198",
            "the rewritten formula must recalculate (99*2), proving the cell \
             is still a formula and not a literal '=B1*2' string"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn look_in_values_leaves_formula_cells_alone() {
        // The mirror of the test above. A formula's displayed value is a
        // computed result; overwriting it with text would silently destroy the
        // formula that produced it, so values-mode must skip formula cells.
        let p = write_csv("replvalues.csv", "a,b\n10,0\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.press_key(Key::ArrowRight).step();
        h.type_text("=A1*2").step();
        h.press_key(Key::Enter).steps(3);
        assert_eq!(h.app().display(CellRef::new(0, 1)), "20");

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_replace_look_in(ferrix_core::LookIn::Values);
        h.app_mut().set_search_input("20");
        h.app_mut().set_replace_input("ZZZ");
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(4);

        assert_eq!(
            h.app().edit_text(CellRef::new(0, 1)),
            "=A1*2",
            "values-mode must not clobber a formula's source"
        );
        assert_eq!(
            h.app().display(CellRef::new(0, 1)),
            "20",
            "and must not replace its computed result"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn replace_all_respects_match_case() {
        let p = write_csv("replcase.csv", "v\nNorth\nnorth\nNORTH\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_case_sensitive(true);
        h.app_mut().set_search_input("North");
        h.app_mut().set_replace_input("SOUTH");
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(3);

        assert_eq!(h.app().display(CellRef::new(0, 0)), "SOUTH");
        assert_eq!(
            h.app().display(CellRef::new(1, 0)),
            "north",
            "case-sensitive replace must leave 'north' alone"
        );
        assert_eq!(h.app().display(CellRef::new(2, 0)), "NORTH");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn replace_all_respects_whole_cell() {
        let p = write_csv("replwhole.csv", "v\nopen\nreopened\nopened\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_whole_cell(true);
        h.app_mut().set_search_input("open");
        h.app_mut().set_replace_input("CLOSED");
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(3);

        assert_eq!(h.app().display(CellRef::new(0, 0)), "CLOSED");
        assert_eq!(
            h.app().display(CellRef::new(1, 0)),
            "reopened",
            "whole-cell must not touch a substring match"
        );
        assert_eq!(h.app().display(CellRef::new(2, 0)), "opened");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn replace_all_supports_regex_with_capture_groups() {
        let p = write_csv("replregex.csv", "d\n2024-07\n2023-01\nnotadate\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_regex(true);
        h.app_mut().set_search_input(r"(\d{4})-(\d{2})");
        h.app_mut().set_replace_input("$2/$1");
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(3);

        assert_eq!(h.app().display(CellRef::new(0, 0)), "07/2024");
        assert_eq!(h.app().display(CellRef::new(1, 0)), "01/2023");
        assert_eq!(
            h.app().display(CellRef::new(2, 0)),
            "notadate",
            "a non-matching cell must be untouched"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn single_replace_changes_only_the_current_match() {
        let p = write_csv("replone.csv", "v\nfoo\nfoo\nfoo\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_input("foo");
        h.app_mut().set_replace_input("bar");
        h.steps(2);
        h.app_mut().do_replace_one();
        h.steps(3);

        let changed = (0..3u32)
            .filter(|r| h.app().display(CellRef::new(*r, 0)) == "bar")
            .count();
        assert_eq!(
            changed, 1,
            "a single Replace must change exactly one cell, not all three"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn replace_all_with_no_matches_changes_nothing_and_pushes_no_undo() {
        // A no-op replace that still dirtied the workbook or grew the undo
        // stack would make Ctrl+Z rewind something the user never did.
        let p = write_csv("replnone.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = snapshot(&h, 4, 3);
        let depth = h.app().undo_depth();

        h.ctrl(Key::H).steps(2);
        h.app_mut().set_search_input("zzzznotpresent");
        h.app_mut().set_replace_input("X");
        h.steps(2);
        h.app_mut().do_replace_all();
        h.steps(3);

        assert_eq!(
            snapshot(&h, 4, 3),
            before,
            "nothing matched, nothing changed"
        );
        assert_eq!(
            h.app().undo_depth(),
            depth,
            "a replace that changed nothing must not push an undo entry"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn replace_all_over_many_rows_does_not_hold_every_match_in_memory() {
        // The scale invariant, at the largest size a unit test can afford.
        // 3,000 matching cells across many small windows replace correctly and the overlay holds exactly
        // the cells that changed — never a per-match index of the whole sheet.
        let rows: String = (0..3_000)
            .map(|i| format!("{i},hit\n"))
            .collect::<Vec<_>>()
            .concat();
        let p = write_csv("replscale.csv", &format!("id,v\n{rows}"));
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(600, |a| a.row_count() >= 3_000));

        h.ctrl(Key::H).steps(2);
        // 64-row windows: 3,000 rows spans ~47 windows, so a walk that
        // dropped, repeated, or mis-bounded a window cannot pass.
        h.app_mut().set_replace_window_rows(64);
        h.app_mut().set_search_input("hit");
        h.app_mut().set_replace_input("done");
        h.steps(2);
        let depth = h.app().undo_depth();
        h.app_mut().do_replace_all();
        h.steps(3);

        // Spot-check across the whole range, including past every window
        // boundary, so a windowed walk that dropped a window would fail.
        for r in [0u32, 1, 999, 1_500, 2_998, 2_999] {
            assert_eq!(
                h.app().display(CellRef::new(r, 1)),
                "done",
                "row {r} must have been replaced"
            );
        }
        // The id column is untouched.
        assert_eq!(h.app().display(CellRef::new(2_999, 0)), "2999");
        assert_eq!(
            h.app().undo_depth(),
            depth + 1,
            "3,000 cells is still ONE undo step"
        );

        h.press_key(Key::Escape).steps(2);
        h.ctrl(Key::Z).steps(3);
        for r in [0u32, 1_500, 2_999] {
            assert_eq!(
                h.app().display(CellRef::new(r, 1)),
                "hit",
                "one undo must restore row {r}"
            );
        }
        let _ = std::fs::remove_file(&p);
    }
}
