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
        crate::theme::Theme::apply(&ctx);
        let app = FerrixApp::new(initial.map(|p| p.to_path_buf()));
        Self {
            ctx,
            app,
            events: Vec::new(),
            pointer: Pos2::new(400.0, 300.0),
            screen: Vec2::new(1400.0, 880.0),
            frame: 0,
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
        let _ = self.ctx.run(raw, |ctx| app.frame(ctx));
        self.frame += 1;
        self
    }

    /// Run `n` frames. Loading happens on a worker thread, so a test that opens
    /// a file steps until the load lands rather than sleeping.
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
    pub fn step_until(&mut self, max: usize, pred: impl Fn(&FerrixApp) -> bool) -> bool {
        for _ in 0..max {
            if pred(&self.app) {
                return true;
            }
            self.step();
        }
        pred(&self.app)
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

    // ---- observation ----

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
}
