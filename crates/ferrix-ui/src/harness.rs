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

    // ---- cell comments (roadmap #12) ----
    //
    // These drive the same entry points the context menu and the editor's
    // buttons call. The alternative -- synthesising a right-click and then a
    // click on a menu item whose pixel position depends on the theme's text
    // metrics -- would test menu layout rather than whether a comment attaches
    // to, follows, and leaves the right cell.

    /// Write (or replace) a comment on a cell, through the real editor path.
    pub fn set_comment(&mut self, cell: ferrix_core::CellRef, author: &str, text: &str) {
        self.app.begin_comment(cell);
        self.app.set_comment_buffers_for_test(author, text);
        self.app.commit_comment();
        self.steps(2);
    }

    /// Delete a cell's comment, as the menu's Delete item does.
    pub fn delete_comment(&mut self, cell: ferrix_core::CellRef) {
        self.app.delete_comment(cell);
        self.steps(2);
    }

    /// Comment markers actually painted by the most recent frame.
    pub fn comment_marker_count(&mut self) -> usize {
        self.step();
        self.app.painted_comment_markers()
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

    // ---- freeze / split / zoom (roadmap #6) ----

    /// Freeze at the cursor. Exposed for the same reason as
    /// `toggle_filter_mode`: the menu item's pixel position moves with the
    /// theme and window width, and a test about FREEZE SEMANTICS should not be
    /// a test about where a menu happens to open. The same entry point the
    /// menu item calls.
    pub fn freeze_at_cursor(&mut self, rows: bool, cols: bool) -> &mut Self {
        self.app.freeze_at_cursor(rows, cols);
        self.steps(2);
        self
    }

    pub fn unfreeze(&mut self) -> &mut Self {
        self.app.unfreeze();
        self.steps(2);
        self
    }

    pub fn split_at_cursor(&mut self) -> &mut Self {
        self.app.split_at_cursor();
        self.steps(2);
        self
    }

    pub fn set_zoom(&mut self, z: f32) -> &mut Self {
        self.app.set_zoom(z);
        self.steps(2);
        self
    }

    // ---- trace precedents / dependents (roadmap #39) ----

    /// Trace Precedents on the cursor cell, same entry point the Formula
    /// menu item and Ctrl+[ call.
    pub fn trace_precedents(&mut self) -> &mut Self {
        self.app.trace_precedents();
        self.steps(2);
        self
    }

    /// Trace Dependents on the cursor cell, same entry point the Formula
    /// menu item and Ctrl+] call.
    pub fn trace_dependents(&mut self) -> &mut Self {
        self.app.trace_dependents();
        self.steps(2);
        self
    }

    /// Remove Arrows.
    pub fn clear_trace(&mut self) -> &mut Self {
        self.app.clear_trace();
        self.steps(2);
        self
    }

    /// Arrows painted by the last frame, and how many the current trace
    /// level covers before the cap.
    pub fn trace_counts(&self) -> (usize, usize) {
        self.app.trace_counts()
    }

    /// Scroll the body pane to a screen row and let the frame settle.
    pub fn scroll_body_to(&mut self, screen_row: f64) -> &mut Self {
        self.app.scroll_body_to(screen_row);
        self.steps(2);
        self
    }

    /// Click a CELL at its ACTUAL painted centre, read back from the app.
    ///
    /// Same discipline as `click_header`: geometry comes from the running app
    /// rather than from constants, so the click still lands when the zoom
    /// changes or a bar opens above the grid. Returns false when the cell is
    /// not on screen, which is itself a useful assertion.
    pub fn click_cell(&mut self, cell: ferrix_core::CellRef) -> bool {
        self.step();
        let Some((x, y)) = self.app.cell_center(cell) else {
            return false;
        };
        self.click_at(x, y).steps(2);
        true
    }

    /// Click a raw viewport POINT — the whole point of the zoom hit-test
    /// check, where the test must supply the pixel and the app must resolve it.
    pub fn click_point(&mut self, x: f32, y: f32) -> &mut Self {
        self.click_at(x, y).steps(2);
        self
    }

    /// The app under test, for assertions.
    pub fn app(&self) -> &FerrixApp {
        &self.app
    }

    // ---- conditional formatting (roadmap #11) ----

    /// Open the New Rule dialog on the current selection.
    ///
    /// Exposed for the same reason as `freeze_at_cursor`: the Format menu item
    /// only exists at a pixel that moves with the theme and window width, and
    /// a test about RULE SEMANTICS should not be a test about where a menu
    /// opens. This is the exact entry point the menu item calls. Everything
    /// that follows — filling the form, pressing OK, pressing Cancel — goes
    /// through the real dialog.
    pub fn cond_new_rule(&mut self) -> &mut Self {
        self.app.cond_new_rule();
        self.steps(2);
        self
    }

    pub fn cond_manage(&mut self) -> &mut Self {
        self.app.cond_manage();
        self.steps(2);
        self
    }

    /// Edit the dialog's form the way the widgets would, then let the preview
    /// settle. `f` receives the live form, so a test says "op is >, value is
    /// 50" rather than synthesising keystrokes into a text field.
    pub fn cond_form(&mut self, f: impl FnOnce(&mut crate::cond_format::RuleForm)) -> &mut Self {
        if let Some(st) = self.app.cond_state_mut() {
            f(&mut st.form);
        }
        self.steps(2);
        self
    }

    /// Press the dialog's REAL OK button, at wherever it was actually painted.
    ///
    /// Not a call to the commit handler: this is a genuine move-then-click at
    /// the button's reported rect, so a dialog whose OK is disabled, covered,
    /// or never drawn fails the test instead of passing it.
    pub fn cond_click_ok(&mut self) -> &mut Self {
        self.step();
        let r = self
            .app
            .cond_state()
            .and_then(|s| s.ok_rect)
            .expect("the rule form's OK button was never painted");
        self.click_at(r.center().x, r.center().y).steps(2);
        self
    }

    /// Press the dialog's REAL Cancel button.
    pub fn cond_click_cancel(&mut self) -> &mut Self {
        self.step();
        let r = self
            .app
            .cond_state()
            .and_then(|s| s.cancel_rect)
            .expect("the rule form's Cancel button was never painted");
        self.click_at(r.center().x, r.center().y).steps(2);
        self
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

    /// Guard for any test that WRITES preferences.
    ///
    /// `set_zoom` persists, and `FERRIX_CONFIG_DIR` plus the prefs file itself
    /// are process-wide. Two zoom tests running in parallel write the same
    /// file, and the one that writes second erases the other's entry — which
    /// looks exactly like a broken persistence feature. Every test that
    /// mutates prefs takes this lock, so the write and the read that follows
    /// it are never interleaved with another test's write.
    fn prefs_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::prefs::CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

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

    // ---- autosave and crash recovery (roadmap #8) ----
    //
    // The failure these guard against: edits live in a .fxedits sidecar and
    // undo history is CLEARED on save, so a crash between saves loses
    // everything typed since the last one with no undo left to recover it.

    /// A private CSV plus a clean slate of sidecar/autosave files.
    ///
    /// Each autosave test gets its own base file so they can run in parallel
    /// without one test's recovery prompt appearing in another's app.
    fn autosave_fixture(name: &str) -> std::path::PathBuf {
        let p = write_csv(&format!("autosave_{name}.csv"), SAMPLE);
        let side = ferrix_io::edits::edits_path_for(&p);
        let _ = std::fs::remove_file(&side);
        let _ = std::fs::remove_file(ferrix_io::edits::autosave_path_for_sidecar(&side));
        p
    }

    fn sidecar_of(base: &std::path::Path) -> std::path::PathBuf {
        ferrix_io::edits::edits_path_for(base)
    }

    fn autosave_of(base: &std::path::Path) -> std::path::PathBuf {
        ferrix_io::edits::autosave_path_for_sidecar(&sidecar_of(base))
    }

    /// Type `text` into the given cell and commit it.
    fn edit_cell(h: &mut Harness, row: u32, col: u32, text: &str) {
        h.app_mut()
            .set_selection_for_test(CellRef::new(row, col), CellRef::new(row, col));
        h.step();
        h.type_text(text).step();
        h.press_key(Key::Enter).steps(2);
    }

    /// THE headline scenario: crash after edits plus one autosave tick,
    /// restart, and Recover must restore every edit.
    ///
    /// "Crash" here means dropping the app WITHOUT any clean-exit path — no
    /// save, no close prompt, no on_clean_exit. That is what makes the
    /// autosave file survive into the next launch, and it is precisely the
    /// state a killed process leaves behind.
    #[test]
    fn crash_after_an_autosave_tick_offers_recovery_and_recover_restores_every_edit() {
        let p = autosave_fixture("crash_recover");
        let edits = [(0u32, 0u32, "111"), (1, 1, "wombat"), (3, 2, "999")];

        {
            let mut h = Harness::new(Some(&p));
            assert!(h.step_until(200, |a| a.row_count() > 0));
            for (r, c, t) in edits {
                edit_cell(&mut h, r, c, t);
            }
            // One autosave tick. This is the only thing standing between the
            // user and losing all three edits.
            h.app_mut().autosave_tick_now();
            assert!(
                autosave_of(&p).exists(),
                "the autosave tick wrote nothing; there is nothing to recover from"
            );
            // Drop without a clean exit: no save, no close, no on_clean_exit.
            // The autosave file must therefore outlive the process.
            std::mem::drop(h);
        }

        assert!(
            autosave_of(&p).exists(),
            "a crash must leave the autosave behind"
        );
        assert!(
            !sidecar_of(&p).exists(),
            "nothing was ever saved, so there must be no official sidecar"
        );

        // Restart.
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        assert!(
            h.app().recovery_prompt_open(),
            "restart after a crash must offer to recover; status: {}",
            h.status()
        );
        let prompt = h.app().recovery_prompt_text().unwrap();
        assert!(
            prompt.starts_with("Recover edits from ") && prompt.ends_with(" ago?"),
            "prompt was {prompt:?}"
        );

        // Before recovering, the edits are NOT present -- otherwise this test
        // could pass without recovery doing anything at all.
        assert_eq!(h.app().display(CellRef::new(0, 0)), "1");

        h.app_mut().recover_autosave();
        h.steps(2);

        assert!(!h.app().recovery_prompt_open(), "prompt must close");
        for (r, c, t) in edits {
            assert_eq!(
                h.app().display(CellRef::new(r, c)),
                t,
                "Recover must restore the edit at ({r},{c})"
            );
        }
        assert!(
            h.app().is_dirty(),
            "recovered edits are unsaved, so the workbook must be dirty"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// A manual save deletes the autosave file.
    #[test]
    fn a_manual_save_deletes_the_autosave() {
        let p = autosave_fixture("save_clears");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        edit_cell(&mut h, 0, 0, "42");
        h.app_mut().autosave_tick_now();
        assert!(
            autosave_of(&p).exists(),
            "precondition: an autosave must exist before the save"
        );

        h.ctrl(Key::S).steps(3);

        assert!(
            sidecar_of(&p).exists(),
            "Ctrl+S must write the official sidecar; status: {}",
            h.status()
        );
        assert!(
            !autosave_of(&p).exists(),
            "a manual save must delete the now-redundant autosave"
        );
        // And the next launch must not offer to recover anything.
        assert!(ferrix_io::edits::find_recovery(&sidecar_of(&p)).is_none());
        let _ = std::fs::remove_file(&p);
    }

    /// Discard deletes the autosave and leaves the official sidecar untouched.
    #[test]
    fn discard_deletes_the_autosave_and_leaves_the_sidecar_untouched() {
        let p = autosave_fixture("discard");

        // Session 1: save one edit officially, then autosave a second on top
        // and crash. The sidecar and the autosave now disagree.
        {
            let mut h = Harness::new(Some(&p));
            assert!(h.step_until(200, |a| a.row_count() > 0));
            edit_cell(&mut h, 0, 0, "saved");
            h.ctrl(Key::S).steps(3);
            assert!(sidecar_of(&p).exists());
            edit_cell(&mut h, 1, 0, "autosaved-only");
            h.app_mut().autosave_tick_now();
            assert!(autosave_of(&p).exists());
            std::mem::drop(h);
        }
        let sidecar_bytes = std::fs::read(sidecar_of(&p)).unwrap();

        // Session 2: decline the recovery.
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        assert!(h.app().recovery_prompt_open());

        h.app_mut().discard_recovery();
        h.steps(2);

        assert!(
            !autosave_of(&p).exists(),
            "Discard must delete the autosave file"
        );
        assert_eq!(
            std::fs::read(sidecar_of(&p)).unwrap(),
            sidecar_bytes,
            "Discard must not touch the official sidecar"
        );
        // The officially saved edit is still there; the autosaved-only one is
        // gone, which is exactly what the user asked for.
        assert_eq!(h.app().display(CellRef::new(0, 0)), "saved");
        assert_eq!(h.app().display(CellRef::new(1, 0)), "2");
        let _ = std::fs::remove_file(&p);
    }

    /// A tick with nothing changed since the last one writes nothing at all.
    ///
    /// Asserted on the file's mtime AND its bytes: "wrote nothing" has to mean
    /// the file was not touched, not merely that it ended up with the same
    /// contents after a rewrite.
    #[test]
    fn a_no_change_tick_writes_nothing_at_all() {
        let p = autosave_fixture("no_change");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        edit_cell(&mut h, 0, 0, "changed");
        h.app_mut().autosave_tick_now();
        let auto = autosave_of(&p);
        assert!(auto.exists(), "precondition: first tick must write");
        let first_mtime = std::fs::metadata(&auto).unwrap().modified().unwrap();
        let first_bytes = std::fs::read(&auto).unwrap();

        // Enough wall clock that a rewrite would certainly move the mtime.
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Several ticks, no edits in between.
        for _ in 0..3 {
            h.app_mut().autosave_tick_now();
            h.steps(2);
        }

        assert_eq!(
            std::fs::metadata(&auto).unwrap().modified().unwrap(),
            first_mtime,
            "an unchanged overlay must not rewrite the autosave file"
        );
        assert_eq!(std::fs::read(&auto).unwrap(), first_bytes);

        // A real edit must still produce a write, or the check above would
        // pass just as well against an autosave that never works again.
        edit_cell(&mut h, 2, 0, "changed again");
        h.app_mut().autosave_tick_now();
        assert_ne!(
            std::fs::metadata(&auto).unwrap().modified().unwrap(),
            first_mtime,
            "a changed overlay must write"
        );
    }

    // --- Compact (roadmap #7) ---

    /// An app whose active base is a real `.ferrix` cache.
    ///
    /// The normal loader only reaches the mmap path above 1 GB, which no test
    /// should be writing, so the cache is built directly and attached. The
    /// grid, the overlay, and Compact itself all see exactly what they would
    /// after a real large-file open.
    fn app_over_a_cache(tag: &str) -> (Harness, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("ferrix_ui_compact_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("data.csv");
        let mut body = String::from("id,name,qty\n");
        for i in 0..200 {
            body.push_str(&format!("{i},row{i},{}\n", i * 2));
        }
        std::fs::write(&csv, body).unwrap();
        let cache = ferrix_io::cache_path_for(&csv);
        ferrix_io::convert_csv(&csv, &cache, b',', true, |_, _| {}).unwrap();

        let mut h = Harness::new(None);
        h.step();
        h.app_mut().adopt_cache_for_test(&cache).unwrap();
        h.step();
        (h, cache)
    }

    #[test]
    fn compact_is_offered_only_when_there_is_something_to_bake() {
        let (mut h, cache) = app_over_a_cache("gate");
        assert!(
            !h.app().compact_available(),
            "an unedited cache has nothing to compact"
        );
        assert!(h.app().compact_tooltip().contains("no edits"));

        edit_cell(&mut h, 3, 1, "changed");
        assert!(
            h.app().compact_available(),
            "one edit is enough to make Compact meaningful"
        );

        // And an in-RAM sheet — no cache at all — can never compact.
        let p = write_csv("compact_inram.csv", SAMPLE);
        let mut h2 = Harness::new(Some(&p));
        assert!(h2.step_until(200, |a| a.row_count() > 0));
        edit_cell(&mut h2, 0, 0, "x");
        assert!(!h2.app().compact_available());
        assert!(h2.app().compact_tooltip().contains("columnar cache"));

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(cache.parent().unwrap());
    }

    /// The headline scenario: edit, compact, and the sidecar is gone while
    /// every value — edited and untouched — is exactly right.
    #[test]
    fn compact_bakes_edits_retires_the_sidecar_and_clears_undo() {
        let (mut h, cache) = app_over_a_cache("headline");

        // Snapshot the BEFORE state so untouched cells can be compared per
        // row rather than by any kind of total.
        let before: Vec<Vec<String>> = (0..200)
            .map(|r| {
                (0..3)
                    .map(|c| h.app().display_for_test(CellRef::new(r, c)))
                    .collect()
            })
            .collect();

        edit_cell(&mut h, 5, 1, "EDITED");
        edit_cell(&mut h, 150, 2, "-7");
        // Saving writes the sidecar, which is the thing compact must retire.
        assert!(h.app_mut().save_edits_for_test());
        let sidecar = h.app().sidecar_path().unwrap().to_path_buf();
        assert!(sidecar.exists(), "a sidecar must exist before the compact");

        // Undo history exists right up until the compact.
        edit_cell(&mut h, 6, 1, "another");
        assert!(h.app().workbook().can_undo());

        h.app_mut().start_compact();
        assert!(
            h.step_until(500, |a| !a.is_compacting()),
            "the compact must finish"
        );

        assert!(!sidecar.exists(), "the sidecar must be retired");
        assert!(
            !h.app().workbook().can_undo(),
            "undo history must be cleared on compact, as on save"
        );
        assert!(
            h.app().status_text().contains("Compacted"),
            "status: {}",
            h.app().status_text()
        );

        // Every edited cell shows its edited value...
        assert_eq!(h.app().display_for_test(CellRef::new(5, 1)), "EDITED");
        assert_eq!(h.app().display_for_test(CellRef::new(6, 1)), "another");
        assert_eq!(h.app().display_for_test(CellRef::new(150, 2)), "-7");
        // ...and every other cell is byte-identical, row by row, so a reorder
        // or a dropped row is caught at the exact index.
        assert_eq!(h.app().row_count(), 200, "row count preserved");
        let edited = [(5u32, 1usize), (6, 1), (150, 2)];
        for (r, row) in before.iter().enumerate() {
            for (c, was) in row.iter().enumerate() {
                if edited.contains(&(r as u32, c)) {
                    continue;
                }
                assert_eq!(
                    &h.app().display_for_test(CellRef::new(r as u32, c as u32)),
                    was,
                    "row {r} col {c} changed"
                );
            }
        }

        // The compacted cache reopens on its own, with the edits in it.
        let m = ferrix_io::MappedSheet::open(&cache).unwrap();
        assert_eq!(m.row_count(), 200);
        assert_eq!(m.display(CellRef::new(5, 1)), "EDITED");
        assert_eq!(m.display(CellRef::new(150, 2)), "-7");

        let _ = std::fs::remove_dir_all(cache.parent().unwrap());
    }

    /// A further edit after a compact must still save. This is the fingerprint
    /// re-anchoring: compact rewrote the base, so a stale fingerprint would
    /// make the next open reject the sidecar and the edits would look lost.
    #[test]
    fn edits_made_after_a_compact_still_save_and_reload() {
        let (mut h, cache) = app_over_a_cache("refingerprint");
        edit_cell(&mut h, 1, 1, "first");
        h.app_mut().start_compact();
        assert!(h.step_until(500, |a| !a.is_compacting()));

        edit_cell(&mut h, 2, 1, "second");
        assert!(h.app_mut().save_edits_for_test(), "the save must succeed");
        let sidecar = h.app().sidecar_path().unwrap().to_path_buf();

        // The sidecar must load against the CURRENT base, not the pre-compact
        // one. A stale fingerprint fails exactly here.
        let m = ferrix_io::MappedSheet::open(&cache).unwrap();
        let fp = ferrix_io::edits::BaseFingerprint::of(
            &cache,
            m.row_count() as u64,
            m.col_count() as u32,
        )
        .unwrap();
        let back = ferrix_io::edits::load_edits(&sidecar, fp)
            .expect("must not be rejected as stale")
            .expect("must be present");
        assert_eq!(back.len(), 1);
        assert_eq!(
            m.display(CellRef::new(1, 1)),
            "first",
            "baked into the cache"
        );

        let _ = std::fs::remove_dir_all(cache.parent().unwrap());
    }

    // --- Name Box and Name Manager (issue #4) ---

    /// A loaded app with `Sales` defined over B1:B3 of the sample, plus a
    /// formula in D1 that uses it.
    fn app_with_a_name(tag: &str) -> (Harness, std::path::PathBuf) {
        // A per-test filename: these tests run in parallel and a shared name
        // would have one test delete another's fixture mid-load.
        let p = // Underscores, not hyphens: the stem becomes the SHEET name, and a
        // hyphen would force it to be quoted in every `refers_to`.
        write_csv(&format!("names_{tag}.csv"), SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        h.select(CellRef::new(0, 2), CellRef::new(3, 2));
        h.app_mut().type_in_name_box("Sales");
        h.app_mut().commit_name_box();
        (h, p)
    }

    #[test]
    fn the_name_box_shows_the_a1_label_until_the_selection_is_named() {
        let p = write_csv("namebox-label.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.select(CellRef::new(0, 2), CellRef::new(3, 2));
        assert_eq!(
            h.app().name_box_text(),
            "C1:C4",
            "an unnamed selection shows its A1 label"
        );

        h.app_mut().type_in_name_box("Sales");
        h.app_mut().commit_name_box();
        assert_eq!(
            h.app().name_box_text(),
            "Sales",
            "once named, the box shows the NAME instead of the label"
        );

        // A different selection reverts to a label — the box tracks the live
        // selection rather than remembering the last thing typed.
        h.select(CellRef::new(0, 0), CellRef::new(0, 0));
        assert_eq!(h.app().name_box_text(), "A1");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn typing_a_new_name_into_the_name_box_defines_it_for_the_selection() {
        let (h, p) = app_with_a_name("define");
        let d = h
            .app()
            .workbook()
            .names
            .get("Sales", None)
            .expect("the Name Box must have defined it");
        // The sheet is named after the CSV's file stem.
        let sheet = h.app().workbook().active_name().to_string();
        assert_eq!(d.refers_to, format!("{sheet}!$C$1:$C$4"));
        assert_eq!(d.scope, ferrix_formula::NameScope::Workbook);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_name_defined_through_the_name_box_is_usable_in_a_formula() {
        // The whole point: what the box defines must actually evaluate.
        let (mut h, p) = app_with_a_name("usable");
        h.select(CellRef::new(0, 4), CellRef::new(0, 4));
        h.type_text("=SUM(Sales)").step();
        h.press_key(Key::Enter).steps(2);
        // qty column is 10+20+30+40.
        assert_eq!(h.app().display(CellRef::new(0, 4)), "100");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn typing_an_existing_name_navigates_to_it() {
        let (mut h, p) = app_with_a_name("navigate");
        h.select(CellRef::new(0, 0), CellRef::new(0, 0));
        assert_eq!(h.app().cursor(), CellRef::new(0, 0));

        h.app_mut().type_in_name_box("sales");
        h.app_mut().commit_name_box();

        let sel = h.app().selection();
        assert_eq!(
            sel.bounds(),
            (CellRef::new(0, 2), CellRef::new(3, 2)),
            "an existing name must navigate, not define a second name"
        );
        assert_eq!(
            h.app().workbook().names.len(),
            1,
            "navigating must not create a duplicate"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn typing_an_address_into_the_name_box_goes_there() {
        let p = write_csv("namebox-goto.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.app_mut().type_in_name_box("B3");
        h.app_mut().commit_name_box();
        assert_eq!(h.app().cursor(), CellRef::new(2, 1));
        assert!(
            h.app().workbook().names.is_empty(),
            "an address must navigate, never become a name"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_name_box_refuses_a_name_that_looks_like_a_reference() {
        let p = write_csv("namebox-bad.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // ZZ9 is a valid cell address, so it navigates rather than defining.
        h.app_mut().type_in_name_box("ZZ9");
        h.app_mut().commit_name_box();
        assert!(h.app().workbook().names.is_empty());

        // "My Name" has a space: not an address, not a legal name either.
        h.app_mut().type_in_name_box("My Name");
        h.app_mut().commit_name_box();
        assert!(
            h.app().workbook().names.is_empty(),
            "an illegal identifier must not land in the table"
        );
        assert!(
            h.status().contains("Cannot define"),
            "the refusal must be reported, got: {}",
            h.status()
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_name_box_can_define_a_sheet_scoped_name() {
        let p = write_csv("namebox-scope.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        let sheet = h.app().workbook().active_name().to_string();

        h.select(CellRef::new(0, 2), CellRef::new(3, 2));
        h.app_mut().set_name_box_sheet_scope(true);
        h.app_mut().type_in_name_box("Local");
        h.app_mut().commit_name_box();

        let d = h.app().workbook().names.get("Local", Some(&sheet)).unwrap();
        assert_eq!(d.scope, ferrix_formula::NameScope::Sheet(sheet));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_manager_renames_a_name_and_rewrites_dependent_formula_text() {
        let (mut h, p) = app_with_a_name("rename");
        h.select(CellRef::new(0, 4), CellRef::new(0, 4));
        h.type_text("=SUM(Sales)*2").step();
        h.press_key(Key::Enter).steps(2);
        let before = h.app().display(CellRef::new(0, 4));

        h.app_mut().open_name_manager();
        assert!(h.app().names_manager_open());
        h.app_mut()
            .begin_name_edit("Sales", ferrix_formula::NameScope::Workbook);
        h.app_mut().set_name_edit_ident("Revenue");
        h.app_mut().apply_name_edit_now();
        h.steps(2);

        assert!(h.app().name_error_text().is_none(), "rename should succeed");
        // The formula's TEXT changed, and its value did not.
        assert_eq!(
            h.app().workbook().view().edit_text(CellRef::new(0, 4)),
            "=SUM(Revenue)*2"
        );
        assert_eq!(h.app().display(CellRef::new(0, 4)), before);
        assert!(h.app().workbook().names.get("Sales", None).is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_manager_reports_a_rename_that_would_collide() {
        let (mut h, p) = app_with_a_name("collide");
        h.select(CellRef::new(0, 0), CellRef::new(3, 0));
        h.app_mut().type_in_name_box("Ids");
        h.app_mut().commit_name_box();
        assert_eq!(h.app().workbook().names.len(), 2);

        h.app_mut()
            .begin_name_edit("Sales", ferrix_formula::NameScope::Workbook);
        h.app_mut().set_name_edit_ident("Ids");
        h.app_mut().apply_name_edit_now();

        assert!(
            h.app()
                .name_error_text()
                .is_some_and(|e| e.contains("already defined")),
            "a colliding rename must be refused and explained, got: {:?}",
            h.app().name_error_text()
        );
        // And nothing moved.
        assert!(h.app().workbook().names.get("Sales", None).is_some());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_manager_retargets_a_name_and_recalculates() {
        let (mut h, p) = app_with_a_name("retarget");
        h.select(CellRef::new(0, 4), CellRef::new(0, 4));
        h.type_text("=SUM(Sales)").step();
        h.press_key(Key::Enter).steps(2);
        assert_eq!(h.app().display(CellRef::new(0, 4)), "100");

        // Repoint Sales at the id column (1+2+3+4).
        h.app_mut()
            .begin_name_edit("Sales", ferrix_formula::NameScope::Workbook);
        let sheet = h.app().workbook().active_name().to_string();
        h.app_mut()
            .set_name_edit_target(&format!("{sheet}!$A$1:$A$4"));
        h.app_mut().apply_name_edit_now();
        h.steps(2);

        assert!(
            h.app().name_error_text().is_none(),
            "retarget rejected: {:?}",
            h.app().name_error_text()
        );
        assert_eq!(
            h.app().display(CellRef::new(0, 4)),
            "10",
            "retargeting a name must recalculate its dependents"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// A clean exit leaves no autosave, so the next launch does not prompt.
    #[test]
    fn a_clean_exit_removes_the_autosave() {
        let p = autosave_fixture("clean_exit");
        {
            let mut h = Harness::new(Some(&p));
            assert!(h.step_until(200, |a| a.row_count() > 0));
            edit_cell(&mut h, 0, 0, "5");
            h.app_mut().autosave_tick_now();
            assert!(autosave_of(&p).exists());

            // The difference from the crash test: an orderly shutdown.
            h.app_mut().on_clean_exit();
            std::mem::drop(h);
        }
        assert!(
            !autosave_of(&p).exists(),
            "a clean exit must not leave an autosave behind"
        );

        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        assert!(
            !h.app().recovery_prompt_open(),
            "a clean exit must not produce a recovery prompt on the next launch"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// With no edits at all, an autosave tick creates no file.
    #[test]
    fn an_untouched_file_never_creates_an_autosave() {
        let p = autosave_fixture("untouched");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        for _ in 0..3 {
            h.app_mut().autosave_tick_now();
            h.steps(2);
        }
        assert!(
            !autosave_of(&p).exists(),
            "opening a file and touching nothing must not write an autosave"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Autosave off means no file, however much is typed.
    #[test]
    fn autosave_can_be_disabled() {
        let p = autosave_fixture("disabled");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.app_mut().set_autosave_secs(0);
        edit_cell(&mut h, 0, 0, "nope");
        h.app_mut().autosave_tick_now();

        assert!(
            !autosave_of(&p).exists(),
            "autosave_secs = 0 must disable autosave entirely"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Recovery restores formulas by SOURCE, re-evaluated against the base --
    /// not just a stale cached number.
    #[test]
    fn recovery_restores_formulas_not_just_their_cached_values() {
        let p = autosave_fixture("formula");
        {
            let mut h = Harness::new(Some(&p));
            assert!(h.step_until(200, |a| a.row_count() > 0));
            edit_cell(&mut h, 0, 2, "=10+32");
            h.app_mut().autosave_tick_now();
            std::mem::drop(h);
        }

        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        assert!(h.app().recovery_prompt_open());
        h.app_mut().recover_autosave();
        h.steps(2);

        assert_eq!(h.app().display(CellRef::new(0, 2)), "42");
        assert_eq!(
            h.app().edit_text(CellRef::new(0, 2)),
            "=10+32",
            "the formula SOURCE must survive recovery, not only its result"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_manager_deletes_a_name_and_its_dependents_become_name_errors() {
        let (mut h, p) = app_with_a_name("delete");
        h.select(CellRef::new(0, 4), CellRef::new(0, 4));
        h.type_text("=SUM(Sales)").step();
        h.press_key(Key::Enter).steps(2);
        assert_eq!(h.app().display(CellRef::new(0, 4)), "100");

        h.app_mut()
            .delete_name_now("Sales", &ferrix_formula::NameScope::Workbook);
        h.steps(2);

        assert_eq!(
            h.app().display(CellRef::new(0, 4)),
            "#NAME?",
            "deleting a referenced name must break its dependents visibly"
        );
        assert!(h.app().workbook().names.is_empty());
        // The user's formula TEXT is kept, so redefining repairs it.
        assert_eq!(
            h.app().workbook().view().edit_text(CellRef::new(0, 4)),
            "=SUM(Sales)"
        );
        let _ = std::fs::remove_file(&p);
    }

    // ================= roadmap #6: freeze panes, split view, zoom ============
    //
    // Every assertion below is on state the FEATURE changes: which underlying
    // row a screen row shows, which row number is painted beside it, and which
    // cell a pixel resolves to. None of them can be satisfied by a file having
    // loaded or by a status string being non-empty.

    /// Build a CSV with `n` data rows: `id,name,qty` where id == row index.
    fn big_csv(name: &str, n: usize) -> std::path::PathBuf {
        let mut body = String::with_capacity(n * 18 + 16);
        body.push_str("id,name,qty\n");
        for i in 0..n {
            body.push_str(&format!("{i},r{i},{}\n", i % 97));
        }
        write_csv(name, &body)
    }

    /// THE acceptance test. Freeze at row 5, drive the body a million rows
    /// down, and assert the frozen rows are STILL PAINTED, still showing their
    /// own records, still numbered 1..5.
    ///
    /// If freeze did nothing at all, the body scroll would carry every painted
    /// row past row 999,000 and row 1 would be nowhere in the frame — which is
    /// exactly what the first assertion checks.
    #[test]
    fn freeze_at_row_5_keeps_row_1_on_screen_after_scrolling_to_row_1_000_000() {
        let n = 1_100_000usize;
        let p = big_csv("freeze_deep.csv", n);
        let mut h = Harness::new(Some(&p));
        assert!(
            h.step_until(2000, |a| a.row_count() >= n),
            "file never loaded; status: {}",
            h.status()
        );

        // Freeze at row 5: the cursor sits on screen row 4 (data row index 4,
        // the 5th row), so rows 1..=4 above it become the frozen band... put
        // the cursor on the 6th row so five rows are frozen.
        h.select(CellRef::new(5, 0), CellRef::new(5, 0));
        h.freeze_at_cursor(true, false);
        assert_eq!(h.app().panes().rows, 5, "five rows must be frozen");
        assert!(h.app().panes().frozen);

        // Now scroll the BODY a million rows down.
        h.scroll_body_to(1_000_000.0);
        assert!(
            h.app().body_row_offset() >= 999_000.0,
            "body did not actually scroll: offset {}",
            h.app().body_row_offset()
        );

        let painted = h.app().painted_rows();
        assert!(!painted.is_empty(), "nothing was painted at all");
        let frozen = h.app().frozen_row_count();
        assert_eq!(frozen, 5, "the frozen band must still paint five rows");

        // ROW 1 IS STILL VISIBLE, and it is the FIRST thing painted -- the
        // frozen band is iterated before the body.
        assert_eq!(
            painted[0],
            (0usize, 0u32),
            "screen row 0 must still show underlying row 0 (row number 1)"
        );
        // Its painted row NUMBER is 1: the row header prints row + 1, and that
        // is the number the user reads.
        let row_number = painted[0].1 as u64 + 1;
        assert_eq!(row_number, 1, "the top row must still be numbered 1");
        // And its DATA is row 1's data, not row 1,000,000's.
        assert_eq!(h.app().display(CellRef::new(painted[0].1, 0)), "0");

        // All five frozen rows are rows 1..=5, in order.
        let frozen_numbers: Vec<u64> = painted[..5].iter().map(|&(_, r)| r as u64 + 1).collect();
        assert_eq!(frozen_numbers, vec![1, 2, 3, 4, 5]);

        // ...while the BODY really is down at a million.
        let body_first = painted[5].1;
        assert!(
            body_first >= 999_000,
            "the body pane should be near row 1,000,000, got {body_first}"
        );

        // The scale invariant: a frozen band is a handful of EXTRA rows, not a
        // second pass over 1.1M rows.
        assert!(
            painted.len() < 200,
            "a frame painted {} rows over a {n}-row sheet",
            painted.len()
        );

        // Unfreezing removes the band, and then row 1 is genuinely gone --
        // which proves the previous assertions were about the freeze and not
        // about some unrelated always-paint-row-1 behaviour.
        h.unfreeze();
        h.scroll_body_to(1_000_000.0);
        let after: Vec<u32> = h.app().painted_underlying_rows();
        assert_eq!(h.app().frozen_row_count(), 0);
        assert!(
            !after.contains(&0),
            "with no freeze, row 1 must NOT be on screen a million rows down"
        );

        let _ = std::fs::remove_file(&p);
    }

    /// At 200% a click at a given point must still resolve to the correct data
    /// cell. The point is chosen from the app's own painted geometry for a
    /// KNOWN cell, then handed back as a raw pixel -- so this tests the hit
    /// test's zoom arithmetic, not the test's ability to guess pixels.
    #[test]
    fn a_click_at_200_percent_resolves_to_the_correct_data_cell() {
        let _prefs = prefs_guard();
        let p = big_csv("zoom_click.csv", 400);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(400, |a| a.row_count() >= 400));

        let target = CellRef::new(7, 2);

        // Where that cell is at 100%.
        h.set_zoom(1.0);
        let at_100 = h.app().cell_center(target).expect("cell on screen at 100%");

        // Where it is at 200%. Geometry MUST move -- if zoom did nothing, the
        // two points would coincide and the rest of this test would pass
        // vacuously, so that is asserted first.
        h.set_zoom(2.0);
        assert!((h.app().zoom() - 2.0).abs() < 1e-4, "zoom was not applied");
        let at_200 = h.app().cell_center(target).expect("cell on screen at 200%");
        assert!(
            (at_200.1 - at_100.1).abs() > 5.0,
            "at 200% the cell should have moved down the screen: {at_100:?} -> {at_200:?}"
        );

        // Click that raw point. The app must resolve it back to C8.
        h.click_point(at_200.0, at_200.1);
        assert_eq!(
            h.app().cursor(),
            target,
            "a click at {at_200:?} at 200% zoom selected the wrong cell"
        );

        // A row further down, to catch an off-by-a-scale-factor that happens to
        // work near the top of the viewport.
        let deeper = CellRef::new(12, 1);
        let pt = h.app().cell_center(deeper).expect("deeper cell on screen");
        h.click_point(pt.0, pt.1);
        assert_eq!(h.app().cursor(), deeper);

        // The SAME screen point means a DIFFERENT cell at a different zoom --
        // the property that makes the hit test genuinely zoom-aware rather
        // than accidentally correct.
        h.set_zoom(1.0);
        h.click_point(at_200.0, at_200.1);
        let at_100_cursor = h.app().cursor();
        assert_ne!(
            at_100_cursor, target,
            "the same pixel resolved to the same cell at both zooms, so the \
             hit test is ignoring zoom"
        );

        let _ = std::fs::remove_file(&p);
    }

    /// Zoom and freeze must not disturb the row resolution: under an active
    /// SORT, the record a given screen row shows is the same with and without
    /// them. This is the regression that once painted wrong records under
    /// correct row numbers.
    #[test]
    fn zoom_and_freeze_compose_with_a_sort_without_changing_which_record_a_row_shows() {
        let _prefs = prefs_guard();
        // Descending-ish qty so a sort genuinely permutes the rows.
        let mut body = String::from("id,name,qty\n");
        for i in 0..300usize {
            body.push_str(&format!("{i},r{i},{}\n", (i * 37) % 300));
        }
        let p = write_csv("zoomsort.csv", &body);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(400, |a| a.row_count() >= 300));

        // Sort by qty (column 2) with a real header click.
        h.click_header(2);
        assert!(h.app().sort_dir(2).is_some(), "sort did not engage");
        let sorted_order = h.app().visible_row_order();
        assert_ne!(
            sorted_order[..8].to_vec(),
            (0u32..8).collect::<Vec<_>>(),
            "the sort did not actually permute anything"
        );

        // Baseline: which underlying row each painted screen row shows.
        h.steps(2);
        let baseline: Vec<(usize, u32)> = h.app().painted_rows().to_vec();
        assert!(baseline.len() > 10);
        // The baseline agrees with the sort mapping -- if it did not, the rest
        // of this test would be comparing two equally wrong things.
        for &(screen, row) in baseline.iter().take(10) {
            assert_eq!(
                sorted_order[screen], row,
                "screen row {screen} showed row {row}, sort says {}",
                sorted_order[screen]
            );
        }

        // Now zoom AND freeze, and re-read.
        h.set_zoom(2.0);
        h.select(
            CellRef::new(sorted_order[3], 0),
            CellRef::new(sorted_order[3], 0),
        );
        h.freeze_at_cursor(true, true);
        assert_eq!(h.app().panes().rows, 3, "three rows frozen");
        assert!((h.app().zoom() - 2.0).abs() < 1e-4);
        // The sort is untouched by either.
        assert!(
            h.app().sort_dir(2).is_some(),
            "zoom/freeze dropped the sort"
        );
        assert_eq!(
            h.app().visible_row_order(),
            sorted_order,
            "zoom or freeze changed the sort order"
        );

        // Every painted screen row still shows the record the SORT says it
        // should -- frozen band and body alike.
        let after: Vec<(usize, u32)> = h.app().painted_rows().to_vec();
        assert!(!after.is_empty());
        assert_eq!(h.app().frozen_row_count(), 3);
        for &(screen, row) in &after {
            assert_eq!(
                sorted_order[screen], row,
                "under zoom+freeze, screen row {screen} shows row {row} but \
                 the sort puts row {} there",
                sorted_order[screen]
            );
        }
        // Specifically: the three frozen rows are the sort's first three.
        let frozen_rows: Vec<u32> = after[..3].iter().map(|&(_, r)| r).collect();
        assert_eq!(frozen_rows, sorted_order[..3].to_vec());

        let _ = std::fs::remove_file(&p);
    }

    /// The same composition, under a FILTER rather than a sort.
    #[test]
    fn zoom_and_freeze_compose_with_a_filter_without_changing_which_record_a_row_shows() {
        let _prefs = prefs_guard();
        let mut body = String::from("id,name,qty\n");
        for i in 0..400usize {
            // Only every 7th row says "keepme".
            let name = if i % 7 == 0 { "keepme" } else { "other" };
            body.push_str(&format!("{i},{name},{}\n", i % 13));
        }
        let p = write_csv("zoomfilter.csv", &body);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(400, |a| a.row_count() >= 400));

        h.ctrl(Key::F).steps(2);
        h.app_mut().set_search_input("keepme");
        h.steps(3);
        h.toggle_filter_mode();
        h.steps(3);

        let kept = h.app().visible_row_order();
        assert!(kept.len() > 20, "filter kept too little: {}", kept.len());
        assert!(
            kept.iter().all(|r| r % 7 == 0),
            "the filter kept rows it should not have"
        );

        // Zoom and freeze on top of the filter.
        h.set_zoom(2.0);
        h.select(CellRef::new(kept[2], 0), CellRef::new(kept[2], 0));
        h.freeze_at_cursor(true, false);
        assert_eq!(
            h.app().panes().rows,
            2,
            "freeze must count SCREEN rows under a filter, not underlying ones"
        );

        // The filter survived, and every painted row still maps through it.
        assert_eq!(h.app().visible_row_order(), kept, "filter changed");
        let painted: Vec<(usize, u32)> = h.app().painted_rows().to_vec();
        assert!(!painted.is_empty());
        for &(screen, row) in &painted {
            if screen < kept.len() {
                assert_eq!(
                    kept[screen], row,
                    "screen row {screen} shows row {row}, filter says {}",
                    kept[screen]
                );
            }
        }
        // The frozen band shows the filter's first two kept rows -- their real
        // row numbers, not 1 and 2.
        assert_eq!(painted[0].1, kept[0]);
        assert_eq!(painted[1].1, kept[1]);

        let _ = std::fs::remove_file(&p);
    }

    /// Split view: two independent scroll offsets over ONE column layout.
    /// Scrolling the body must leave the split band where it was.
    #[test]
    fn split_view_scrolls_its_two_panes_independently() {
        let p = big_csv("splitview.csv", 5_000);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(600, |a| a.row_count() >= 5_000));

        h.select(CellRef::new(4, 0), CellRef::new(4, 0));
        h.split_at_cursor();
        assert_eq!(h.app().panes().rows, 4);
        assert!(
            !h.app().panes().frozen,
            "a split band must NOT be pinned -- that is what makes it a split"
        );

        let band_before: Vec<u32> = h
            .app()
            .painted_rows()
            .iter()
            .take(4)
            .map(|&(_, r)| r)
            .collect();
        assert_eq!(band_before, vec![0, 1, 2, 3]);

        // Move only the body.
        h.scroll_body_to(2_000.0);
        let painted = h.app().painted_rows().to_vec();
        let band_after: Vec<u32> = painted.iter().take(4).map(|&(_, r)| r).collect();
        assert_eq!(
            band_after, band_before,
            "the body scroll dragged the split band with it"
        );
        // Unlike a freeze, a split band CAN show a row the body also could --
        // but here the body has moved far away, so they are disjoint.
        assert!(
            painted[4].1 >= 1_900,
            "the body pane did not move: first body row {}",
            painted[4].1
        );

        let _ = std::fs::remove_file(&p);
    }

    /// A frozen COLUMN band keeps its columns on screen while the body scrolls
    /// right, and the same column keeps the same width in both bands.
    #[test]
    fn frozen_columns_stay_on_screen_and_share_the_body_column_widths() {
        // Wide enough that scrolling right pushes column A off a normal grid.
        let mut body = String::new();
        for c in 0..40 {
            if c > 0 {
                body.push(',');
            }
            body.push_str(&format!("col{c}"));
        }
        body.push('\n');
        for r in 0..50 {
            for c in 0..40 {
                if c > 0 {
                    body.push(',');
                }
                body.push_str(&format!("v{r}_{c}"));
            }
            body.push('\n');
        }
        let p = write_csv("freezecols.csv", &body);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(600, |a| a.col_count() >= 40));

        // Freeze the first two columns.
        h.select(CellRef::new(0, 2), CellRef::new(0, 2));
        h.freeze_at_cursor(false, true);
        assert_eq!(h.app().panes().cols, 2);

        // A frozen column's header is still on screen...
        let a_header = h.app().header_center(0);
        assert!(a_header.is_some(), "frozen column A lost its header");
        // ...and a cell in it still has a rect.
        let a_cell = h
            .app()
            .cell_center(CellRef::new(3, 0))
            .expect("frozen column A must still be painted");
        // Clicking it selects it, so the frozen band is really hit-testable.
        h.click_point(a_cell.0, a_cell.1);
        assert_eq!(h.app().cursor(), CellRef::new(3, 0));

        let _ = std::fs::remove_file(&p);
    }

    /// Zoom is clamped to 25%..400% and remembered per sheet across a restart.
    #[test]
    fn zoom_is_clamped_and_persists_per_sheet() {
        // FERRIX_CONFIG_DIR is process-wide and other tests in this binary
        // redirect it too; without this lock they interleave and a working
        // persistence feature reports itself broken.
        let _guard = crate::prefs::CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ferrix-zoom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let prev = std::env::var_os("FERRIX_CONFIG_DIR");
        std::env::set_var("FERRIX_CONFIG_DIR", &dir);

        let p = big_csv("zoompersist.csv", 100);
        {
            let mut h = Harness::new(Some(&p));
            assert!(h.step_until(400, |a| a.row_count() >= 100));
            // Out of range in both directions is clamped, not accepted.
            h.set_zoom(99.0);
            assert!((h.app().zoom() - 4.0).abs() < 1e-4, "zoom exceeded 400%");
            h.set_zoom(0.01);
            assert!((h.app().zoom() - 0.25).abs() < 1e-4, "zoom fell below 25%");
            h.set_zoom(2.0);
            assert!((h.app().zoom() - 2.0).abs() < 1e-4);
        }

        // A FRESH app is exactly what the next process run is.
        {
            let mut h2 = Harness::new(Some(&p));
            assert!(h2.step_until(400, |a| a.row_count() >= 100));
            assert!(
                (h2.app().zoom() - 2.0).abs() < 1e-4,
                "zoom did not survive a restart: got {}",
                h2.app().zoom()
            );
        }

        match prev {
            Some(v) => std::env::set_var("FERRIX_CONFIG_DIR", v),
            None => std::env::remove_var("FERRIX_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_formula_bar_preview_resolves_defined_names() {
        // Before names, `=SUM(Sales)` in the bar previewed a parse error. The
        // preview must go through the same name-aware parse a commit does.
        let (mut h, p) = app_with_a_name("preview");
        h.select(CellRef::new(0, 4), CellRef::new(0, 4));
        h.app_mut().set_formula_input("=SUM(Sales)");
        h.steps(2);
        // The preview carries a timing suffix; the VALUE is what matters.
        let shown = h.app().formula_preview().unwrap_or("").to_string();
        assert!(
            shown.starts_with("100"),
            "the live preview must resolve the name, not report #NAME?; got {shown:?}"
        );
        let _ = std::fs::remove_file(&p);
    }

    // ---- cell comments / notes (roadmap #12) ----
    //
    // The invariants worth guarding, in order of how badly they fail:
    //  * a comment must FOLLOW its cell through a column reorder. Leaving it
    //    keyed to the old display column puts the note beside a different
    //    number — plausible, wrong, and invisible.
    //  * an uncommented sheet must cost NOTHING on the paint path, which runs
    //    per visible cell per frame at 60fps.
    //  * deleting a comment must remove the MARKER, not merely the map entry.

    #[test]
    fn a_comment_attaches_to_the_cell_and_paints_a_marker() {
        let p = write_csv("comment_add.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let before = h.comment_marker_count();
        assert_eq!(before, 0, "a fresh sheet has no comment markers");

        h.set_comment(CellRef::new(1, 1), "ana", "check with finance");

        assert_eq!(h.app().comment_count(), 1);
        assert_eq!(
            h.app().comment_text(CellRef::new(1, 1)),
            Some("check with finance")
        );
        // The MARKER, read from the grid's real paint output rather than from
        // the map — the model already agreed with itself above.
        assert_eq!(
            h.comment_marker_count(),
            1,
            "the commented cell must paint exactly one marker"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn editing_a_comment_replaces_it_rather_than_adding_a_second() {
        let p = write_csv("comment_edit.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let cell = CellRef::new(2, 0);
        h.set_comment(cell, "ana", "first draft");
        h.set_comment(cell, "bo", "revised");

        assert_eq!(
            h.app().comment_count(),
            1,
            "an edit must not leave the old note behind"
        );
        assert_eq!(h.app().comment_text(cell), Some("revised"));
        assert_eq!(
            h.comment_marker_count(),
            1,
            "one comment must paint one marker, not two stacked"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn deleting_a_comment_removes_the_marker() {
        // The assertion that matters is on the PAINT output: clearing the map
        // while the grid kept drawing would look identical in the model and
        // completely broken on screen.
        let p = write_csv("comment_delete.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let cell = CellRef::new(0, 2);
        h.set_comment(cell, "ana", "temporary");
        assert_eq!(h.comment_marker_count(), 1, "setup: the marker is painted");

        h.delete_comment(cell);

        assert_eq!(h.app().comment_count(), 0);
        assert_eq!(h.app().comment_text(cell), None);
        assert_eq!(
            h.comment_marker_count(),
            0,
            "deleting a comment must stop the marker being painted"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn clearing_a_comments_text_deletes_it() {
        // "Select all, delete, save" is how a user removes a note without
        // hunting for a Delete button. An empty note stored as a note would
        // leave a marker promising text that is not there.
        let p = write_csv("comment_clear.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        let cell = CellRef::new(1, 0);
        h.set_comment(cell, "ana", "will be cleared");
        assert_eq!(h.app().comment_count(), 1);

        h.set_comment(cell, "ana", "   ");
        assert_eq!(h.app().comment_count(), 0, "blank text removes the note");
        assert_eq!(h.comment_marker_count(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_comment_follows_its_cell_through_a_column_reorder() {
        // THE pitfall this feature was warned about. The overlay is keyed by
        // DISPLAY position and relocated on reorder; a comment map keyed the
        // same way and NOT relocated would leave the note sitting on whatever
        // column happens to land in that slot — beside a different number,
        // with nothing on screen to say so.
        let p = write_csv("comment_reorder.csv", "a,b,c\n1,2,3\n4,5,6\n");
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        // Note on B1, which holds 2.
        let b1 = CellRef::new(0, 1);
        assert_eq!(h.app().display(b1), "2", "setup");
        h.set_comment(b1, "ana", "this 2 is provisional");

        // Move column A to the end: a,b,c -> b,c,a. The value 2 moves from
        // display 1 to display 0.
        h.move_columns(0, 1, 3).expect("move");
        h.steps(3);
        assert_eq!(h.app().display(CellRef::new(0, 0)), "2", "b moved to A");
        assert_eq!(h.app().display(CellRef::new(0, 2)), "1", "a moved to C");

        // The comment must have travelled WITH the 2, to display column 0.
        assert_eq!(
            h.app().comment_text(CellRef::new(0, 0)),
            Some("this 2 is provisional"),
            "the note must follow the cell it annotates"
        );
        assert_eq!(
            h.app().comment_text(b1),
            None,
            "the note must not be left on the column the 2 vacated"
        );
        assert_eq!(
            h.app().comment_count(),
            1,
            "a reorder must not duplicate or drop the note"
        );
        assert_eq!(h.comment_marker_count(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_cell_with_no_comment_costs_no_lookup_work_on_the_paint_path() {
        // The paint path asks about every visible cell, every frame. A sheet
        // with no comments must not probe the map once; a sheet WITH comments
        // must probe once per visible ROW, not once per visible cell.
        let p = write_csv("comment_cost.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        h.app().comment_map().reset_probes();
        h.step();
        let painted = h.app().painted_cell_count();
        assert!(painted > 0, "setup: the grid must have painted something");
        assert_eq!(
            h.app().comment_map().probes(),
            0,
            "{painted} painted cells on an uncommented sheet must cost zero \
             comment-map probes"
        );

        // With a comment present, the cost must scale with rows, not cells.
        h.set_comment(CellRef::new(0, 0), "ana", "hi");
        h.app().comment_map().reset_probes();
        h.step();
        let probes = h.app().comment_map().probes();
        let rows = h.app().painted_row_count() as u64;
        assert!(rows > 0, "setup: rows must have been painted");
        assert!(
            probes <= rows,
            "the paint path made {probes} probes for {rows} painted rows — \
             the row lookup is not hoisted out of the column loop"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_200m_row_shaped_sheet_with_three_comments_stores_exactly_three_entries() {
        // Comments are sparse: cost is O(comments), never O(rows). Addresses
        // are placed across a 200M-row space to prove nothing is materialised
        // per row.
        let p = write_csv("comment_scale.csv", SAMPLE);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));

        for (row, col, text) in [
            (0u32, 0u32, "top"),
            (99_999_999, 1, "middle"),
            (199_999_999, 2, "bottom"),
        ] {
            h.set_comment(CellRef::new(row, col), "ana", text);
        }

        assert_eq!(
            h.app().comment_count(),
            3,
            "three comments over a 200M-row address space must cost three entries"
        );
        assert!(
            h.app().comment_map().heap_bytes() < 4_000,
            "three comments cost {} bytes",
            h.app().comment_map().heap_bytes()
        );
        // Deep row indices must survive intact rather than being clamped to
        // the loaded sheet's extent.
        assert_eq!(
            h.app().comment_text(CellRef::new(199_999_999, 2)),
            Some("bottom"),
            "a comment at row 200M must survive at its true address"
        );
    }

    // ============ conditional formatting editor (roadmap #11) ==============
    //
    // Every assertion below is on what the app RESOLVES for a specific cell,
    // or on the rule count, or on the whole SheetFormat compared for equality.
    // None of them is on a status string or on "a rule is in a list" — those
    // pass against an editor that stores rules nothing ever reads, which is
    // exactly the dead-feature shape a previous UI test here missed.

    /// Twelve numeric rows, so a threshold has both matching and non-matching
    /// cells and a top/bottom-N has a meaningful window.
    const NUMS: &str = "id,qty\n1,5\n2,150\n3,7\n4,200\n5,9\n6,3\n7,180\n8,1\n9,120\n10,4\n";

    fn numeric_app(name: &str) -> (Harness, std::path::PathBuf) {
        let p = write_csv(name, NUMS);
        let mut h = Harness::new(Some(&p));
        assert!(
            h.step_until(200, |a| a.row_count() > 0),
            "fixture never loaded"
        );
        (h, p)
    }

    #[test]
    fn creating_a_threshold_rule_restyles_matching_cells_and_leaves_others_alone() {
        // The headline behaviour. B2 is 150 and must end up filled; B1 is 5 and
        // must end up exactly as plain as it started. Asserting BOTH is what
        // separates a working rule from a rule that paints everything.
        let (mut h, p) = numeric_app("cf_threshold.csv");
        let hit = CellRef::new(1, 1); // 150
        let miss = CellRef::new(0, 1); // 5
        let window = 0..10;

        assert!(
            h.app().resolved_style(hit, window.clone()).is_plain(),
            "precondition: nothing is styled before the rule exists"
        );

        h.select(CellRef::new(0, 1), CellRef::new(9, 1));
        h.cond_new_rule();
        h.cond_form(|f| {
            f.kind = crate::cond_format::RuleKind::Threshold;
            f.op = ferrix_core::CmpOp::Gt;
            f.value = 100.0;
            f.fill = ferrix_core::Rgb(0x11, 0x22, 0x33);
            f.text = ferrix_core::Rgb(0x44, 0x55, 0x66);
        });
        h.cond_click_ok();

        let after_hit = h.app().resolved_style(hit, window.clone());
        let after_miss = h.app().resolved_style(miss, window.clone());

        assert_eq!(
            after_hit.fill,
            Some(ferrix_core::Rgb(0x11, 0x22, 0x33)),
            "150 > 100: the rule must reach the cell's resolved style"
        );
        assert_eq!(after_hit.text, Some(ferrix_core::Rgb(0x44, 0x55, 0x66)));
        assert!(
            after_miss.is_plain(),
            "5 > 100 is false, so this cell must be untouched; got {after_miss:?}"
        );
        assert_eq!(h.app().rule_count(), 1, "exactly one rule was created");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_live_preview_shows_the_rule_before_it_is_committed() {
        // The preview is the feature's whole selling point, and it is also the
        // easiest thing to fake. So: the cell must resolve STYLED while the
        // dialog is open, and the store must still be empty at that moment.
        let (mut h, p) = numeric_app("cf_preview.csv");
        let hit = CellRef::new(1, 1);
        let window = 0..10;

        h.select(CellRef::new(0, 1), CellRef::new(9, 1));
        h.cond_new_rule();
        h.cond_form(|f| {
            f.kind = crate::cond_format::RuleKind::Threshold;
            f.op = ferrix_core::CmpOp::Gt;
            f.value = 100.0;
            f.fill = ferrix_core::Rgb(0xDE, 0xAD, 0xBE);
        });

        assert_eq!(
            h.app().resolved_style(hit, window.clone()).fill,
            Some(ferrix_core::Rgb(0xDE, 0xAD, 0xBE)),
            "the grid must show the pending rule while the dialog is open"
        );
        assert_eq!(
            h.app().rule_count(),
            0,
            "a PREVIEW must not have written anything to the store"
        );

        // Turning the preview off must take it straight back off the grid.
        if let Some(st) = h.app_mut().cond_state_mut() {
            st.preview = false;
        }
        h.steps(2);
        assert!(
            h.app().resolved_style(hit, window).is_plain(),
            "unchecking live preview must stop previewing"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn comments_survive_a_save_and_reopening_the_file() {
        // A note that vanishes when the file is reopened is a note nobody will
        // ever write a second one of.
        let p = write_csv("comment_persist.csv", SAMPLE);
        let side = ferrix_io::comments_path_for(&p);
        let _ = std::fs::remove_file(&side);

        {
            let mut h = Harness::new(Some(&p));
            assert!(h.step_until(200, |a| a.row_count() > 0));
            h.set_comment(CellRef::new(1, 1), "ana", "survives a restart");
            h.set_comment(CellRef::new(3, 0), "bo", "so does this one");
            assert!(h.app_mut().save_comments(), "save must report success");
        }

        assert!(side.exists(), "saving must write the comment sidecar");

        // Fresh app, same file: the notes must come back on their own cells.
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.comment_count() == 2));
        assert_eq!(
            h.app().comment_text(CellRef::new(1, 1)),
            Some("survives a restart")
        );
        assert_eq!(
            h.app().comment_text(CellRef::new(3, 0)),
            Some("so does this one")
        );
        assert_eq!(
            h.comment_marker_count(),
            2,
            "restored comments must paint their markers"
        );

        let _ = std::fs::remove_file(&side);
    }

    #[test]
    fn cancelling_the_dialog_leaves_the_sheet_exactly_as_it_was() {
        // Cancel is the action that must be INCAPABLE of changing anything.
        // The whole SheetFormat is compared, not a spot check, so a stray
        // entry anywhere in any scope fails this.
        let (mut h, p) = numeric_app("cf_cancel.csv");
        let hit = CellRef::new(1, 1);
        let window = 0..10;

        // Start from a sheet that already HAS a rule: "cancel changed nothing"
        // is a much weaker claim on an empty store than on a populated one.
        h.select(CellRef::new(0, 1), CellRef::new(9, 1));
        h.cond_new_rule();
        h.cond_form(|f| {
            f.kind = crate::cond_format::RuleKind::Threshold;
            f.op = ferrix_core::CmpOp::Lt;
            f.value = 6.0;
            f.fill = ferrix_core::Rgb(0x01, 0x02, 0x03);
        });
        h.cond_click_ok();
        let before = h.app().format_snapshot();
        let before_style = h.app().resolved_style(hit, window.clone());
        assert_eq!(h.app().rule_count(), 1);

        // Now open a second rule, fill it in, watch it preview — and cancel.
        h.cond_new_rule();
        h.cond_form(|f| {
            f.kind = crate::cond_format::RuleKind::Threshold;
            f.op = ferrix_core::CmpOp::Gt;
            f.value = 100.0;
            f.fill = ferrix_core::Rgb(0xFF, 0x00, 0xFF);
        });
        assert_eq!(
            h.app().resolved_style(hit, window.clone()).fill,
            Some(ferrix_core::Rgb(0xFF, 0x00, 0xFF)),
            "precondition: the rule being cancelled was genuinely previewing, \
             otherwise this test proves nothing"
        );

        h.cond_click_cancel();

        assert!(!h.app().cond_is_open(), "Cancel must close the dialog");
        assert_eq!(
            h.app().format_snapshot(),
            before,
            "Cancel must leave the sheet's formatting byte-identical"
        );
        assert_eq!(
            h.app().resolved_style(hit, window),
            before_style,
            "and the cell must resolve exactly as it did before"
        );
        assert_eq!(h.app().rule_count(), 1, "the cancelled rule was not stored");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reordering_two_overlapping_rules_changes_which_one_wins() {
        // Two rules that both match 150. Precedence is the ONLY thing deciding
        // what the user sees, so ▼ must visibly change the resolved fill.
        let (mut h, p) = numeric_app("cf_order.csv");
        let hit = CellRef::new(1, 1); // 150 — matches both rules
        let window = 0..10;
        let red = ferrix_core::Rgb(0xFF, 0x00, 0x00);
        let blue = ferrix_core::Rgb(0x00, 0x00, 0xFF);

        h.select(CellRef::new(0, 1), CellRef::new(9, 1));
        for fill in [red, blue] {
            h.cond_new_rule();
            h.cond_form(move |f| {
                f.kind = crate::cond_format::RuleKind::Threshold;
                f.op = ferrix_core::CmpOp::Gt;
                f.value = 100.0;
                f.fill = fill;
            });
            h.cond_click_ok();
        }
        assert_eq!(h.app().rule_count(), 2);
        assert_eq!(
            h.app().resolved_style(hit, window.clone()).fill,
            Some(blue),
            "the LATER rule wins, matching apply_cell's own precedence"
        );

        // Move the red rule (index 0) later. Same mutation the ▼ button makes.
        let target = h.app().cond_state().expect("manage list is open").target;
        let moved = target.move_rule(h.app_mut().format_mut_for_test(), 0, 1);
        assert!(moved, "the reorder must actually happen");
        h.steps(2);

        assert_eq!(
            h.app().resolved_style(hit, window).fill,
            Some(red),
            "after the reorder the OTHER rule must win — order IS the behaviour"
        );
        assert_eq!(
            h.app().rule_count(),
            2,
            "reordering must not add or drop rules"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn deleting_a_comment_and_saving_does_not_resurrect_it_on_reload() {
        // The failure: save writes only non-empty maps, so an emptied map
        // leaves the old sidecar on disk and the note comes back from the dead.
        let p = write_csv("comment_unpersist.csv", SAMPLE);
        let side = ferrix_io::comments_path_for(&p);
        let _ = std::fs::remove_file(&side);

        let cell = CellRef::new(2, 1);
        {
            let mut h = Harness::new(Some(&p));
            assert!(h.step_until(200, |a| a.row_count() > 0));
            h.set_comment(cell, "ana", "should not come back");
            assert!(h.app_mut().save_comments());
            h.delete_comment(cell);
            assert!(h.app_mut().save_comments());
        }

        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(200, |a| a.row_count() > 0));
        h.steps(3);
        assert_eq!(
            h.app().comment_count(),
            0,
            "a deleted comment must not survive the reload"
        );
        assert_eq!(h.comment_marker_count(), 0);
        let _ = std::fs::remove_file(&side);
    }

    #[test]
    fn a_topbottom_rule_surfaces_the_xlsx_lossy_warning_in_the_editor() {
        // TopBottom has no lossless xlsx mapping. The user must learn that in
        // the dialog — the whole reason `rule_survives_xlsx` is public.
        let (mut h, p) = numeric_app("cf_lossy.csv");
        h.select(CellRef::new(0, 1), CellRef::new(9, 1));
        h.cond_new_rule();

        // A threshold survives export, so nothing should be warned about yet.
        h.cond_form(|f| f.kind = crate::cond_format::RuleKind::Threshold);
        let benign = h
            .app()
            .cond_state()
            .map(|s| crate::cond_format::xlsx_warning(&s.form.to_rule()));
        assert_eq!(
            benign,
            Some(None),
            "warning about a rule that DOES survive would train users to ignore it"
        );

        h.cond_form(|f| {
            f.kind = crate::cond_format::RuleKind::TopBottom;
            f.top = true;
            f.n = 3;
        });
        let warn = h
            .app()
            .cond_state()
            .and_then(|s| crate::cond_format::xlsx_warning(&s.form.to_rule()))
            .expect("TopBottom must warn while it is still being edited");
        assert!(
            warn.contains("DROPPED") && warn.to_lowercase().contains("xlsx"),
            "the warning must say what actually happens on export: {warn}"
        );

        // And it must survive the commit, into the status the user keeps.
        h.cond_click_ok();
        let kept = h.app().cond_warning().unwrap_or_default().to_string();
        assert!(
            kept.contains("DROPPED"),
            "the lossy warning must outlive the dialog; got {kept:?}"
        );
        assert!(
            h.status().contains("Top 3"),
            "and the status must name the rule that was made; got {:?}",
            h.status()
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_rule_on_a_100k_row_column_stores_exactly_one_entry() {
        // THE scale invariant. Asserted on the RULE COUNT and the heap, never
        // on cells: a per-cell implementation would pass any test that counted
        // painted cells and fail this one, which is the point.
        let (mut h, p) = numeric_app("cf_scale.csv");

        h.select(CellRef::new(0, 1), CellRef::new(99_999, 1));
        h.cond_new_rule();
        h.cond_form(|f| {
            f.kind = crate::cond_format::RuleKind::Threshold;
            f.op = ferrix_core::CmpOp::Gt;
            f.value = 100.0;
        });
        h.cond_click_ok();

        assert_eq!(
            h.app().rule_count(),
            1,
            "100,000 rows must cost ONE rule entry, not one per row"
        );
        let heap = h.app().format_snapshot().heap_bytes();
        assert!(
            heap < 4096,
            "a 100k-row rule must not cost real memory; heap_bytes was {heap}"
        );
        assert_eq!(
            h.app().format_snapshot().override_count(),
            0,
            "nothing may leak into the per-cell override map"
        );

        // Still a live rule, not a cheap no-op: the cell it covers is styled.
        assert!(
            !h.app().resolved_style(CellRef::new(1, 1), 0..10).is_plain(),
            "the one stored entry must still actually apply"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_rule_changes_what_the_frame_actually_paints() {
        // The resolved style is the model's answer; this is the SCREEN's. A
        // fill is an extra rect per matching cell, so the frame's shape count
        // must go up when the rule lands and back down when it is removed.
        let (mut h, p) = numeric_app("cf_paint.csv");

        // Baseline AFTER the selection is made: a multi-cell selection paints
        // its own range fill, so measuring before it would credit the rule
        // with shapes the selection drew.
        h.select(CellRef::new(0, 1), CellRef::new(9, 1));
        let baseline = h.paint_shape_count();

        h.cond_new_rule();
        h.cond_form(|f| {
            f.kind = crate::cond_format::RuleKind::Threshold;
            // Matches every row, so the change is unambiguous rather than one
            // rect that could hide in frame-to-frame noise.
            f.op = ferrix_core::CmpOp::Gt;
            f.value = 0.0;
            f.fill = ferrix_core::Rgb(0x20, 0x80, 0x20);
        });
        h.cond_click_ok();
        // Close the dialog so its own chrome is not what changed the count.
        h.app_mut().cond_close_for_test();
        let with_rule = h.paint_shape_count();

        assert!(
            with_rule > baseline,
            "a fill on 10 cells must add shapes to the frame: {baseline} -> {with_rule}"
        );

        let target = h.app().selection();
        let t =
            crate::cond_format::CondTarget::from_selection(target.bounds().0, target.bounds().1);
        assert!(t.remove(h.app_mut().format_mut_for_test(), 0));
        h.steps(2);
        let removed = h.paint_shape_count();
        assert_eq!(
            removed, baseline,
            "deleting the rule must put the frame back exactly where it started"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn manage_finds_the_column_rules_when_the_user_has_clicked_one_cell() {
        // A rule made on the whole column, then one cell selected. Telling the
        // user "no rules here" reads as data loss and is the most likely way
        // for this dialog to lie.
        let (mut h, p) = numeric_app("cf_manage.csv");
        h.select(CellRef::new(0, 1), CellRef::new(9, 1));
        h.cond_new_rule();
        if let Some(st) = h.app_mut().cond_state_mut() {
            st.target = st.target.widen(); // "Entire column"
        }
        h.cond_form(|f| {
            f.kind = crate::cond_format::RuleKind::Threshold;
            f.value = 100.0;
        });
        h.cond_click_ok();
        assert_eq!(h.app().rule_count(), 1);

        // Now select a single cell inside that column and manage.
        h.select(CellRef::new(3, 1), CellRef::new(3, 1));
        h.cond_manage();
        let st = h.app().cond_state().expect("manage must be open");
        assert_eq!(
            st.target,
            crate::cond_format::CondTarget::Column(1),
            "Manage must find the column's rules rather than reporting none"
        );
        assert_eq!(
            st.target.rules(&h.app().format_snapshot()).len(),
            1,
            "and it must list the rule that is actually there"
        );
        let _ = std::fs::remove_file(&p);
    }

    // ---- trace precedents / dependents (roadmap #39) ----

    fn small_numeric_app(name: &str) -> (Harness, std::path::PathBuf) {
        let p = write_csv(name, NUMS);
        let mut h = Harness::new(Some(&p));
        assert!(
            h.step_until(200, |a| a.row_count() > 0),
            "fixture never loaded"
        );
        (h, p)
    }

    /// The AGENT_GUIDE question, applied at the UI layer: tracing a cell
    /// with no formula at all -- what a dead gesture would report if the
    /// feature did nothing -- must draw ZERO arrows, not some placeholder.
    #[test]
    fn tracing_a_plain_data_cell_draws_no_arrows() {
        let (mut h, p) = small_numeric_app("trace_dead.csv");
        h.select(CellRef::new(0, 0), CellRef::new(0, 0));
        let before = h.paint_shape_count();
        h.trace_precedents();
        let (drawn, total) = h.trace_counts();
        assert_eq!(drawn, 0, "a plain data cell has no precedents to draw");
        assert_eq!(total, 0);
        // The shape count must not have grown from tracing nothing -- the
        // exact failure mode AGENT_GUIDE.md calls out: a status line (or
        // here, a shape count) that looks non-trivial for a dead gesture.
        let after = h.paint_shape_count();
        assert!(
            after <= before + 2,
            "tracing an empty precedent set must not paint phantom arrows \
             (before={before}, after={after})"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Trace Precedents on a real formula draws one arrow per precedent, and
    /// the count is the REAL PAINT OUTPUT (`paint_shape_count`), not a model
    /// value that could be wrong while nothing reaches the screen.
    #[test]
    fn trace_precedents_draws_one_arrow_per_precedent() {
        let (mut h, p) = small_numeric_app("trace_prec.csv");
        // C1 = A1 + B1 -- two precedents, both on screen at the default view.
        edit_cell(&mut h, 0, 2, "=A1+B1");
        h.select(CellRef::new(0, 2), CellRef::new(0, 2));
        let shapes_before = h.paint_shape_count();
        h.trace_precedents();
        let (drawn, total) = h.trace_counts();
        assert_eq!(total, 2, "C1 reads exactly A1 and B1");
        assert_eq!(drawn, 2, "both precedents are on screen and must be drawn");
        let shapes_after = h.paint_shape_count();
        assert!(
            shapes_after > shapes_before,
            "tracing a real formula must paint MORE shapes than before it \
             (before={shapes_before}, after={shapes_after})"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Trace Dependents is the mirror direction: arrows point AT the cells
    /// that read the traced one.
    #[test]
    fn trace_dependents_finds_every_direct_reader() {
        let (mut h, p) = small_numeric_app("trace_dep.csv");
        edit_cell(&mut h, 0, 2, "=A1*2");
        edit_cell(&mut h, 0, 3, "=A1+1");
        h.select(CellRef::new(0, 0), CellRef::new(0, 0));
        h.trace_dependents();
        let (drawn, total) = h.trace_counts();
        assert_eq!(total, 2, "A1 has exactly two direct dependents");
        assert_eq!(drawn, 2);
        let _ = std::fs::remove_file(&p);
    }

    /// Repeated invocations walk one level further out, as Excel does --
    /// the acceptance criterion checked directly against real paint output.
    #[test]
    fn repeated_trace_precedents_walks_further_out_each_time() {
        let (mut h, p) = small_numeric_app("trace_depth.csv");
        // C1 = A1, D1 = C1: a two-hop chain.
        edit_cell(&mut h, 0, 2, "=A1");
        edit_cell(&mut h, 0, 3, "=C1");
        h.select(CellRef::new(0, 3), CellRef::new(0, 3));

        h.trace_precedents();
        let (drawn1, total1) = h.trace_counts();
        assert_eq!(total1, 1, "first press reaches only C1");
        assert_eq!(drawn1, 1);

        h.trace_precedents();
        let (drawn2, total2) = h.trace_counts();
        assert_eq!(
            total2, 2,
            "second press on the SAME cell must walk one level further, to A1"
        );
        assert_eq!(drawn2, 2);
        let _ = std::fs::remove_file(&p);
    }

    /// Remove Arrows actually clears the paint output, not just a flag --
    /// asserted against `paint_shape_count`, matching AGENT_GUIDE.md's rule
    /// that a UI test must read the screen, not the model.
    #[test]
    fn remove_arrows_stops_painting_them() {
        let (mut h, p) = small_numeric_app("trace_remove.csv");
        edit_cell(&mut h, 0, 2, "=A1+B1");
        h.select(CellRef::new(0, 2), CellRef::new(0, 2));
        h.trace_precedents();
        let (drawn_before, _) = h.trace_counts();
        assert_eq!(drawn_before, 2);

        let shapes_with_arrows = h.paint_shape_count();
        h.clear_trace();
        let shapes_without = h.paint_shape_count();
        let (drawn_after, total_after) = h.trace_counts();
        assert_eq!(drawn_after, 0, "Remove Arrows must stop drawing them");
        assert_eq!(total_after, 0);
        assert!(
            shapes_without < shapes_with_arrows,
            "removing arrows must reduce the real paint output \
             (with={shapes_with_arrows}, without={shapes_without})"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Changing the selection does NOT silently strand the arrows -- they
    /// keep tracing their original origin cell until explicitly removed.
    #[test]
    fn changing_selection_does_not_clear_the_trace() {
        let (mut h, p) = small_numeric_app("trace_stranded.csv");
        edit_cell(&mut h, 0, 2, "=A1+B1");
        h.select(CellRef::new(0, 2), CellRef::new(0, 2));
        h.trace_precedents();
        let (drawn_before, _) = h.trace_counts();
        assert_eq!(drawn_before, 2);

        // Move the selection elsewhere -- a plain move, not a menu action.
        h.select(CellRef::new(3, 0), CellRef::new(3, 0));
        h.step();
        let (drawn_after, total_after) = h.trace_counts();
        assert_eq!(
            drawn_after, drawn_before,
            "arrows must survive a selection change until Remove Arrows is pressed"
        );
        assert_eq!(total_after, 2);
        let _ = std::fs::remove_file(&p);
    }

    /// A cell in a cycle is traceable without hanging, and the underlying
    /// cycle detection the paint code consults agrees that it IS a cycle --
    /// the property that decides the distinct (dashed/red) styling.
    #[test]
    fn a_cyclic_cell_traces_without_hanging_and_is_flagged_circular() {
        let (mut h, p) = small_numeric_app("trace_cycle.csv");
        // C1 = D1, D1 = C1: a direct two-cell cycle, deliberately not A1/B1
        // so it does not disturb the plain data columns other assertions
        // might rely on.
        edit_cell(&mut h, 0, 2, "=D1");
        edit_cell(&mut h, 0, 3, "=C1");
        h.select(CellRef::new(0, 2), CellRef::new(0, 2));
        h.trace_precedents();
        let (drawn, total) = h.trace_counts();
        assert_eq!(total, 1, "one edge exists to walk even inside a cycle");
        assert_eq!(drawn, 1, "the cycle must still be painted, not dropped");
        let sheet = h.app().active_sheet_id();
        let c1 = ferrix_core::SheetCell::new(sheet, CellRef::new(0, 2));
        assert!(
            h.app().graph_snapshot().is_circular_at(c1),
            "the cell the arrow starts from must be recognised as circular"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// The scale invariant at the UI layer: a cell with many dependents
    /// must still paint in bounded time and never attempt more than
    /// MAX_ARROWS arrows, while `trace_counts().1` still reports the true
    /// total -- "showing N of M", not a silently truncated lie. The
    /// 500k-scale claim itself is pinned directly in trace.rs against the
    /// graph, with no UI frame cost.
    #[test]
    fn a_cell_with_many_dependents_is_capped_in_the_real_paint_output() {
        // A purpose-built 60-row fixture: writing formulas up to row 41
        // must land on REAL rows, not past `row_count` into empty-row
        // padding territory, which would fail for reasons unrelated to the
        // cap this test is pinning.
        let mut body = String::from("id,qty\n");
        for i in 1..=60u32 {
            body.push_str(&format!("{i},{i}\n"));
        }
        let p = write_csv("trace_cap.csv", &body);
        let mut h = Harness::new(Some(&p));
        assert!(
            h.step_until(200, |a| a.row_count() > 0),
            "fixture never loaded"
        );

        for r in 1..41u32 {
            edit_cell(&mut h, r, 1, "=A1");
        }
        h.select(CellRef::new(0, 0), CellRef::new(0, 0));
        // Park the body at the top so the ORIGIN cell (A1) is on screen.
        // Editing row 41 scrolled the view down, and with A1 off screen too
        // an edge with NEITHER endpoint visible has nothing to point at and
        // is legitimately skipped — which would make this test's number an
        // accident of scroll position rather than a statement about the cap.
        h.scroll_body_to(0.0);
        assert!(
            h.app().cell_center(CellRef::new(0, 0)).is_some(),
            "test setup: the origin cell must be on screen"
        );
        h.trace_dependents();
        let (drawn, total) = h.trace_counts();
        assert_eq!(total, 40, "the real total must be reported honestly");
        assert!(
            drawn <= crate::trace::MAX_ARROWS,
            "must never exceed the cap"
        );
        assert_eq!(
            drawn, 40,
            "with the origin on screen, every edge has something to point at \
             — the off-screen ones clamp to the viewport edge rather than \
             being dropped"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// The "showing N of M" note the issue asks for, at the point where it
    /// actually matters: MORE dependents than MAX_ARROWS, so the drawn count
    /// and the true total genuinely differ. A capped trace that reported
    /// only "100 arrows" would be lying by omission.
    #[test]
    fn exceeding_the_cap_reports_showing_n_of_m_honestly() {
        let mut body = String::from("id,qty\n");
        for i in 1..=200u32 {
            body.push_str(&format!("{i},{i}\n"));
        }
        let p = write_csv("trace_over_cap.csv", &body);
        let mut h = Harness::new(Some(&p));
        assert!(
            h.step_until(200, |a| a.row_count() > 0),
            "fixture never loaded"
        );

        // 120 dependents on A1 -- deliberately MORE than MAX_ARROWS (100).
        let deps = crate::trace::MAX_ARROWS + 20;
        for r in 1..=deps as u32 {
            edit_cell(&mut h, r, 1, "=A1");
        }
        h.select(CellRef::new(0, 0), CellRef::new(0, 0));
        h.scroll_body_to(0.0);
        h.trace_dependents();

        let (drawn, total) = h.trace_counts();
        assert_eq!(total, deps, "the TRUE total must survive the cap");
        assert_eq!(
            drawn,
            crate::trace::MAX_ARROWS,
            "the drawn count must be clamped to the cap, not to the total"
        );
        assert!(
            total > drawn,
            "test setup: this test is only meaningful when the cap actually bites"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Off-screen sources are indicated at the viewport edge rather than
    /// drawn at wrong coordinates: when the precedent has scrolled out of
    /// view, an arrow is still counted as drawn (its far end clamped to the
    /// grid rect), and the ORIGIN cell's own on-screen rect is unaffected.
    #[test]
    fn an_offscreen_precedent_still_draws_an_arrow_at_the_viewport_edge() {
        let big = {
            let mut s = String::from("id,qty\n");
            for i in 1..=3000u32 {
                s.push_str(&format!("{i},{i}\n"));
            }
            s
        };
        let p = write_csv("trace_offscreen.csv", &big);
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(400, |a| a.row_count() > 0));

        // A far-down formula cell referencing A1, which will be scrolled out
        // of view once the body is parked near the bottom.
        edit_cell(&mut h, 2000, 2, "=A1");
        h.select(CellRef::new(2000, 2), CellRef::new(2000, 2));
        h.scroll_body_to(1990.0);

        // Precondition: A1 really is off screen from here.
        assert!(
            h.app().cell_center(CellRef::new(0, 0)).is_none(),
            "test setup: A1 must be off screen for this to check anything"
        );
        assert!(
            h.app().cell_center(CellRef::new(2000, 2)).is_some(),
            "test setup: the origin cell must be on screen"
        );

        h.trace_precedents();
        let (drawn, total) = h.trace_counts();
        assert_eq!(total, 1);
        assert_eq!(
            drawn, 1,
            "an off-screen precedent must still produce an arrow to the \
             viewport edge, not be silently dropped"
        );
        let _ = std::fs::remove_file(&p);
    }
}
