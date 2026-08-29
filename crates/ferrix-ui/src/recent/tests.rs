//! Tests for recent files, templates and session restore (#45).

use super::*;
use crate::prefs::Prefs;

// ---------------------------------------------------------------------------
// The zoom-keying bug this issue exists to fix
// ---------------------------------------------------------------------------

/// The criterion. Two DIFFERENT workbooks, each with a sheet called "Sheet1",
/// each zoomed differently. Under the old sheet-name-only keying the second
/// `set_zoom` overwrote the first and both reported the same number, so this
/// is the assertion that fails on the pre-fix code.
#[test]
fn identically_named_sheets_in_different_workbooks_keep_separate_zooms() {
    let a = Path::new("C:/books/alpha.csv");
    let b = Path::new("C:/books/beta.csv");

    let mut p = Prefs::default();
    p.set_zoom(a, "Sheet1", 2.0);
    p.set_zoom(b, "Sheet1", 0.5);

    assert_eq!(p.zoom_of(a, "Sheet1"), 2.0, "workbook A lost its zoom");
    assert_eq!(p.zoom_of(b, "Sheet1"), 0.5, "workbook B lost its zoom");
    // Two workbooks, two entries — not one that the second call clobbered.
    assert_eq!(p.zoom.len(), 2, "the two workbooks shared one zoom entry");

    // And it survives the file round trip, which is where a key that cannot
    // be re-split would collapse the two back into one.
    let re = Prefs::parse(&p.to_text());
    assert_eq!(re.zoom_of(a, "Sheet1"), 2.0, "A's zoom did not round-trip");
    assert_eq!(re.zoom_of(b, "Sheet1"), 0.5, "B's zoom did not round-trip");
}

/// The same sheet name in the same workbook is still ONE entry — the fix must
/// not turn every zoom into a new row.
#[test]
fn rezooming_the_same_sheet_replaces_rather_than_appends() {
    let a = Path::new("/data/book.csv");
    let mut p = Prefs::default();
    p.set_zoom(a, "Sheet1", 2.0);
    p.set_zoom(a, "Sheet1", 3.0);
    assert_eq!(p.zoom.len(), 1);
    assert_eq!(p.zoom_of(a, "Sheet1"), 3.0);
}

/// 100% is the default, and the default is stored as ABSENCE — otherwise the
/// file accrues an entry for every sheet the user ever glanced at. Behaviour
/// preserved from before the re-keying.
#[test]
fn resetting_to_one_hundred_percent_removes_the_entry() {
    let a = Path::new("/data/book.csv");
    let mut p = Prefs::default();
    p.set_zoom(a, "Sheet1", 2.0);
    assert_eq!(p.zoom.len(), 1);
    p.set_zoom(a, "Sheet1", 1.0);
    assert!(p.zoom.is_empty(), "100% must be absence, not a stored 1.0");
    assert_eq!(p.zoom_of(a, "Sheet1"), 1.0);
}

/// The hostile round trip. The composite key has to survive a path with
/// spaces, a Windows drive letter and its colon, backslashes, and a SHEET NAME
/// THAT CONTAINS THE SEPARATOR ITSELF — the case that silently mis-splits if
/// the separator is not escaped.
#[test]
fn a_hostile_path_and_sheet_name_round_trip_without_colliding() {
    let nasty_path = Path::new(r"C:\Users\Ana Maria\My Documents\q1 = final|v2.xlsx");
    let nasty_sheet = "sheet | with = separators and spaces";

    let mut p = Prefs::default();
    p.set_zoom(nasty_path, nasty_sheet, 2.0);
    // A decoy that a naive split would confuse with the entry above.
    p.set_zoom(Path::new(r"C:\Users\Ana Maria\My Documents\q1"), "x", 0.5);

    let re = Prefs::parse(&p.to_text());
    assert_eq!(
        re.zoom_of(nasty_path, nasty_sheet),
        2.0,
        "the hostile key did not survive the round trip: {:?}",
        re.zoom
    );
    assert_eq!(
        re.zoom_of(Path::new(r"C:\Users\Ana Maria\My Documents\q1"), "x"),
        0.5,
        "the decoy entry was lost or merged"
    );
    assert_eq!(re.zoom.len(), 2);
    assert_eq!(re, p, "the whole prefs value did not round-trip");
}

/// Backward compatibility, DOCUMENTED AND PINNED: an old-format `zoom.<sheet>`
/// line has no workbook in it, and there is no way to invent one. Guessing a
/// workbook would apply a remembered zoom to the wrong file, which is exactly
/// the bug being fixed. So old lines are DROPPED — the user loses a zoom
/// level, re-zooms once, and is correct from then on.
///
/// What must NOT happen is a parse failure or a lost *other* setting, so this
/// asserts the rest of the file still lands.
#[test]
fn an_old_format_zoom_line_is_dropped_and_does_not_break_the_rest_of_the_file() {
    let old = "theme = \"light\"\n\
               show_empty_rows = true\n\
               zoom.Sheet1 = 2\n\
               zoom.quarterly report = 0.5\n\
               autosave_secs = 45\n";
    let p = Prefs::parse(old);

    assert!(
        p.zoom.is_empty(),
        "an old sheet-only zoom key was adopted under some invented workbook: {:?}",
        p.zoom
    );
    // Everything else in the file survived the unknown keys.
    assert_eq!(p.theme, Some(crate::theme::ThemeMode::Light));
    assert!(p.show_empty_rows);
    assert_eq!(p.autosave_secs, Some(45));
    // And no workbook picks the orphaned zoom up.
    assert_eq!(p.zoom_of(Path::new("/anything.csv"), "Sheet1"), 1.0);
}

// ---------------------------------------------------------------------------
// Recent list
// ---------------------------------------------------------------------------

#[test]
fn the_most_recently_opened_file_is_first_and_is_never_duplicated() {
    let mut list = Vec::new();
    touch(&mut list, Path::new("/a.csv"));
    touch(&mut list, Path::new("/b.csv"));
    touch(&mut list, Path::new("/a.csv"));

    assert_eq!(list.len(), 2, "reopening a file duplicated it");
    assert_eq!(list[0].path, PathBuf::from("/a.csv"));
    assert_eq!(list[1].path, PathBuf::from("/b.csv"));
}

#[test]
fn the_list_is_capped_and_drops_the_oldest_not_the_newest() {
    let mut list = Vec::new();
    for i in 0..MAX_RECENT + 5 {
        touch(&mut list, Path::new(&format!("/f{i}.csv")));
    }
    assert_eq!(list.len(), MAX_RECENT);
    // The newest survived and the oldest went.
    let newest = format!("/f{}.csv", MAX_RECENT + 4);
    assert_eq!(list[0].path, PathBuf::from(&newest));
    assert!(
        !list.iter().any(|e| e.path == Path::new("/f0.csv")),
        "the cap dropped something other than the oldest"
    );
}

/// The criterion that a disconnected drive is not a deletion. A missing file
/// stays in the list, is reported unavailable so the UI can grey it, and goes
/// only when the user removes it.
#[test]
fn a_missing_file_is_kept_and_flagged_rather_than_dropped() {
    let dir = std::env::temp_dir().join(format!("ferrix-recent-miss-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.join("here.csv");
    std::fs::write(&real, "a,b\n1,2\n").unwrap();
    let gone = dir.join("on-a-disconnected-drive.csv");

    let mut list = Vec::new();
    touch(&mut list, &real);
    touch(&mut list, &gone);

    // Both are still listed — nothing was pruned for being unreachable.
    assert_eq!(list.len(), 2, "an unreachable file was silently dropped");
    let miss = list.iter().find(|e| e.path == gone).unwrap();
    let have = list.iter().find(|e| e.path == real).unwrap();
    assert!(
        !miss.is_available(),
        "a nonexistent file reported available"
    );
    assert!(have.is_available(), "an existing file reported unavailable");
    // The full path is what the hover shows, not just the file name.
    assert_eq!(miss.full_path(), gone.display().to_string());
    assert_eq!(miss.label(), "on-a-disconnected-drive.csv");

    // Removable: the user's explicit choice takes it out, and only it.
    remove(&mut list, &gone);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].path, real);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recent_entries_and_their_sessions_round_trip_through_the_prefs_file() {
    let mut p = Prefs::default();
    touch(
        &mut p.recent,
        Path::new(r"C:\Users\Ana Maria\q1 = final.xlsx"),
    );
    touch(&mut p.recent, Path::new("/mnt/share/b.csv"));
    set_session(
        &mut p.recent,
        Path::new(r"C:\Users\Ana Maria\q1 = final.xlsx"),
        Session {
            anchor: (11, 3),
            cursor: (40, 7),
            scroll_row: 123.5,
            scroll_col_px: 64.0,
            frozen_rows: 2,
            frozen_cols: 1,
            frozen: true,
        },
    );

    let re = Prefs::parse(&p.to_text());
    assert_eq!(re, p, "the recent list did not round-trip");
    let s = session_of(&re.recent, Path::new(r"C:\Users\Ana Maria\q1 = final.xlsx"));
    assert_eq!(s.cursor, (40, 7));
    assert_eq!(s.scroll_row, 123.5);
    assert_eq!(s.frozen_rows, 2);
    assert!(s.frozen);
}

/// A corrupt or hand-edited file must not be able to make the app allocate an
/// unbounded list, and the entries it does describe must still land.
#[test]
fn a_malformed_recent_section_falls_back_rather_than_exploding() {
    for bad in [
        "recent.notanumber.path = /a.csv\n",
        "recent.999999999.path = /a.csv\n",
        "recent.0.cursor = 1,2\n",
        "recent.0.scroll = wat\n",
        "recent.0.panes = 1,2\n",
        "recent.\n",
        "recent.0 = /a.csv\n",
    ] {
        let p = Prefs::parse(bad);
        assert!(
            p.recent.len() <= MAX_RECENT,
            "{bad:?} produced {} entries",
            p.recent.len()
        );
    }
    // The oversized index is ignored, not clamped into a real slot.
    assert!(Prefs::parse("recent.999999999.path = /a.csv\n")
        .recent
        .is_empty());

    // A sparse file: only entry 2 is described, and the phantom 0 and 1 that
    // indexing has to create must not survive as blank rows in the UI.
    let p = Prefs::parse("recent.2.path = /only.csv\n");
    assert_eq!(p.recent.len(), 1);
    assert_eq!(p.recent[0].path, PathBuf::from("/only.csv"));
}

#[test]
fn templates_are_offered_and_are_usable_as_written() {
    let ts = templates();
    assert!(ts.len() >= 3, "the start screen must offer templates");
    for t in ts {
        assert!(!t.name.is_empty());
        assert!(!t.description.is_empty(), "{} has no description", t.name);
        assert!(!t.headers.is_empty(), "{} has no headers", t.name);
        // Every seed row must fit the header row, or the template opens
        // ragged and the formulas point at the wrong columns.
        for (i, r) in t.rows.iter().enumerate() {
            assert_eq!(
                r.len(),
                t.headers.len(),
                "{} row {i} has {} cells against {} headers",
                t.name,
                r.len(),
                t.headers.len()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic write, and the missing/malformed fallback
// ---------------------------------------------------------------------------

/// Redirect `FERRIX_CONFIG_DIR` at a private temp dir for the duration of a
/// test, restoring it after. The env var is process-wide, so the caller must
/// already hold `CONFIG_ENV_LOCK`.
struct ConfigDirGuard {
    dir: PathBuf,
    prev: Option<std::ffi::OsString>,
}

impl ConfigDirGuard {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ferrix-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("FERRIX_CONFIG_DIR");
        std::env::set_var("FERRIX_CONFIG_DIR", &dir);
        Self { dir, prev }
    }
    fn prefs_file(&self) -> PathBuf {
        self.dir.join("prefs.toml")
    }
}

impl Drop for ConfigDirGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("FERRIX_CONFIG_DIR", v),
            None => std::env::remove_var("FERRIX_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The atomicity criterion, stated as the thing a crash must not do.
///
/// A reader hammering the prefs file while writes churn must only ever see a
/// COMPLETE file — the previous one or the new one, never a prefix. The
/// assertion is on the parsed CONTENT, not on `save()` returning Ok: a
/// truncating writer also returns Ok, and the corruption only shows up in what
/// the next process reads back.
#[test]
fn a_write_interrupted_at_any_instant_leaves_the_previous_prefs_fully_readable() {
    let _lock = crate::prefs::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let g = ConfigDirGuard::new("atomic");

    // A previous complete file, with a marker no partial write could forge.
    let mut before = Prefs {
        autosave_secs: Some(4242),
        ..Prefs::default()
    };
    for i in 0..MAX_RECENT {
        let f = format!("/before/f{i}.csv");
        touch(&mut before.recent, Path::new(&f));
        before.set_zoom(Path::new(&f), "a sheet with spaces", 2.0);
    }
    before.save().expect("seed save");
    assert_eq!(Prefs::load().autosave_secs, Some(4242));

    // The new value being written repeatedly. Sizeable, so a non-atomic
    // writer would leave a wide window in which a prefix is visible.
    let mut after = before.clone();
    after.autosave_secs = Some(7);

    let path = g.prefs_file();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rstop = stop.clone();
    let reader = std::thread::spawn(move || {
        let mut reads = 0usize;
        let mut bad = Vec::new();
        while !rstop.load(std::sync::atomic::Ordering::Relaxed) {
            // A momentarily absent file (mid-rename), or Windows denying
            // access during it, is a sharing violation and not a torn file.
            if let Ok(text) = std::fs::read_to_string(&path) {
                let p = Prefs::parse(&text);
                // Every observation must be ONE of the two complete files.
                // A torn write shows up as the wrong autosave value, a short
                // recent list, or lost zoom entries.
                let complete = (p.autosave_secs == Some(4242) || p.autosave_secs == Some(7))
                    && p.recent.len() == MAX_RECENT
                    && p.zoom.len() == MAX_RECENT;
                if !complete {
                    bad.push(format!(
                        "autosave={:?} recent={} zoom={} bytes={}",
                        p.autosave_secs,
                        p.recent.len(),
                        p.zoom.len(),
                        text.len()
                    ));
                }
                reads += 1;
            }
        }
        (reads, bad)
    });

    for i in 0..60 {
        let p = if i % 2 == 0 { &after } else { &before };
        p.save().expect("save");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (reads, bad) = reader.join().unwrap();

    assert!(
        bad.is_empty(),
        "reader observed {} torn prefs file(s): {:?}",
        bad.len(),
        &bad[..bad.len().min(5)]
    );
    assert!(
        reads > 0,
        "the reader never read anything; the test proved nothing"
    );

    // No scratch left behind in the user's config directory.
    let leftovers: Vec<String> = std::fs::read_dir(&g.dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

/// A missing prefs file, and a malformed one, must both start the app on
/// defaults rather than failing. Asserted on the VALUE, so a fallback that
/// silently invented a theme or a recent list would fail here.
#[test]
fn a_missing_or_malformed_prefs_file_falls_back_to_defaults() {
    let _lock = crate::prefs::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let g = ConfigDirGuard::new("fallback");

    // Missing: the directory exists, the file does not.
    assert_eq!(
        Prefs::load(),
        Prefs::default(),
        "a missing prefs file was not the default"
    );

    // Malformed, in several flavours — including a file truncated mid-line,
    // which is exactly what a non-atomic writer used to be able to leave.
    let complete = {
        let mut p = Prefs::default();
        touch(&mut p.recent, Path::new("/a.csv"));
        p.set_zoom(Path::new("/a.csv"), "Sheet1", 2.0);
        p.to_text()
    };
    let torn = complete[..complete.len() / 2].to_string();
    for bad in [
        "".to_string(),
        "\0\0\0\0".to_string(),
        "!!! not a config at all\n".to_string(),
        "recent.0.path\n".to_string(),
        "zoom.=\n".to_string(),
        torn,
    ] {
        std::fs::write(g.prefs_file(), &bad).unwrap();
        let p = Prefs::load();
        // Never panics, never invents a setting the file did not contain.
        assert_eq!(p.theme, None, "{bad:?} invented a theme");
        assert!(!p.show_empty_rows, "{bad:?} invented show_empty_rows");
        assert!(
            p.recent.len() <= MAX_RECENT,
            "{bad:?} produced {} recent entries",
            p.recent.len()
        );
    }

    // A binary file the app has no business reading at all.
    std::fs::write(g.prefs_file(), [0xffu8; 512]).unwrap();
    let p = Prefs::load();
    assert_eq!(p.theme, None);
    assert!(p.recent.is_empty());
}

// ---------------------------------------------------------------------------
// End to end, through the REAL app via the headless harness
// ---------------------------------------------------------------------------

fn fixture_csv(name: &str, rows: usize) -> PathBuf {
    let dir = std::env::temp_dir().join("ferrix_recent_fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let mut body = String::from("id,name,qty\n");
    for i in 0..rows {
        body.push_str(&format!("{i},r{i},{}\n", i % 97));
    }
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// The zoom-keying criterion, driven through the REAL app rather than through
/// `Prefs` alone. The CSV loader names the sheet after the file stem, so two
/// files with the SAME NAME in different directories give two workbooks whose
/// sheets are identically named — the exact shape of the reported bug.
#[test]
fn two_workbooks_with_a_same_named_sheet_remember_different_zooms_in_the_real_app() {
    use crate::harness::Harness;
    let _lock = crate::prefs::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = ConfigDirGuard::new("zoomapp");

    let base = std::env::temp_dir().join(format!("ferrix-two-books-{}", std::process::id()));
    let (da, db) = (base.join("a"), base.join("b"));
    std::fs::create_dir_all(&da).unwrap();
    std::fs::create_dir_all(&db).unwrap();
    let mut body = String::from("id,name,qty\n");
    for i in 0..120 {
        body.push_str(&format!("{i},r{i},{}\n", i % 97));
    }
    let pa = da.join("book.csv");
    let pb = db.join("book.csv");
    std::fs::write(&pa, &body).unwrap();
    std::fs::write(&pb, &body).unwrap();

    {
        // Zoom file A to 200%.
        let mut ha = Harness::new(Some(&pa));
        assert!(ha.step_until(400, |a| a.row_count() >= 120));
        let sheet_a = ha.app().active_sheet_name().to_string();
        ha.set_zoom(2.0);

        // Zoom file B to 50%.
        let mut hb = Harness::new(Some(&pb));
        assert!(hb.step_until(400, |a| a.row_count() >= 120));
        // Both files really do call their sheet the same thing — otherwise
        // this test would pass without exercising the bug at all.
        assert_eq!(
            hb.app().active_sheet_name(),
            sheet_a,
            "the two fixtures must share a sheet name for this test to mean anything"
        );
        hb.set_zoom(0.5);
    }

    // Fresh apps — exactly what the next process run is.
    {
        let mut ha = Harness::new(Some(&pa));
        assert!(ha.step_until(400, |a| a.row_count() >= 120));
        assert!(
            (ha.app().zoom() - 2.0).abs() < 1e-4,
            "file A came back at {} — file B's zoom bled across",
            ha.app().zoom()
        );
        let mut hb = Harness::new(Some(&pb));
        assert!(hb.step_until(400, |a| a.row_count() >= 120));
        assert!(
            (hb.app().zoom() - 0.5).abs() < 1e-4,
            "file B came back at {} — file A's zoom bled across",
            hb.app().zoom()
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}

/// The session-restore criterion, end to end: selection, scroll, zoom and
/// frozen panes all come back when the file is reopened in a FRESH app.
///
/// Every assertion is against a value that differs from the default, and the
/// pre-checks assert the state really moved off the defaults first — so a
/// feature that did nothing fails here rather than passing vacuously.
#[test]
fn reopening_a_file_restores_selection_scroll_zoom_and_frozen_panes() {
    use crate::harness::Harness;
    use ferrix_core::CellRef;
    let _lock = crate::prefs::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = ConfigDirGuard::new("session");

    let p = fixture_csv("session_restore.csv", 800);

    let (want_cursor, want_scroll, want_frozen) = {
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(400, |a| a.row_count() >= 800));

        // A state that is nothing like the default in all four respects.
        h.select(CellRef::new(40, 2), CellRef::new(52, 2));
        h.freeze_at_cursor(true, true);
        h.set_zoom(2.0);
        h.scroll_body_to(300.0);
        h.steps(2);

        let cursor = h.app().cursor();
        let scroll = h.app().scroll_row_offset();
        let frozen = h.app().panes();
        // Sanity: the state really did move off the defaults, or the restore
        // assertions below would pass vacuously.
        assert_ne!(cursor, CellRef::new(0, 0), "the cursor never moved");
        assert!(scroll > 10.0, "the scroll never moved: {scroll}");
        assert!(frozen.rows > 0, "nothing was frozen");
        assert!((h.app().zoom() - 2.0).abs() < 1e-4);

        // A clean exit is what persists the session.
        h.app_mut().on_clean_exit();
        (cursor, scroll, frozen)
    };

    // Fresh app on the same file: the user lands back where they were.
    let mut h2 = Harness::new(Some(&p));
    assert!(h2.step_until(400, |a| a.row_count() >= 800));
    h2.steps(2);

    assert_eq!(
        h2.app().cursor(),
        want_cursor,
        "the selection was not restored"
    );
    assert!(
        (h2.app().zoom() - 2.0).abs() < 1e-4,
        "the zoom was not restored: {}",
        h2.app().zoom()
    );
    assert_eq!(
        h2.app().panes().rows,
        want_frozen.rows,
        "the frozen row band was not restored"
    );
    assert_eq!(
        h2.app().panes().cols,
        want_frozen.cols,
        "the frozen column band was not restored"
    );
    assert!(
        (h2.app().scroll_row_offset() - want_scroll).abs() < 1.0,
        "the scroll position was not restored: wanted {want_scroll}, got {}",
        h2.app().scroll_row_offset()
    );

    let _ = std::fs::remove_file(&p);
}

/// Opening a file puts it at the head of the recent list in the real app, and
/// the list survives into the next run.
#[test]
fn opening_a_file_records_it_in_the_recent_list_and_it_survives_a_restart() {
    use crate::harness::Harness;
    let _lock = crate::prefs::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = ConfigDirGuard::new("recentapp");

    let p = fixture_csv("recent_records.csv", 60);
    {
        let mut h = Harness::new(Some(&p));
        assert!(h.step_until(400, |a| a.row_count() >= 60));
        assert_eq!(
            h.app().recent().first().map(|e| e.path.clone()),
            Some(p.clone()),
            "the opened file did not reach the head of the recent list"
        );
        h.app_mut().on_clean_exit();
    }
    // Next run, launched with NO file: the start screen is showing and the
    // list is there to pick from.
    let h2 = Harness::new(None);
    assert!(
        h2.app().showing_start_screen(),
        "a launch with no file must offer the start screen"
    );
    assert_eq!(
        h2.app().recent().first().map(|e| e.path.clone()),
        Some(p.clone()),
        "the recent list did not survive the restart"
    );

    let _ = std::fs::remove_file(&p);
}

/// A template opens a real, evaluated workbook — the formulas in it are
/// computed by the engine, not stored as text.
#[test]
fn a_template_opens_a_workbook_whose_formulas_actually_evaluate() {
    use crate::harness::Harness;
    use ferrix_core::CellRef;
    let _lock = crate::prefs::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = ConfigDirGuard::new("template");

    let budget = templates()
        .iter()
        .position(|t| t.name == "Budget")
        .expect("the Budget template");

    let mut h = Harness::new(None);
    assert!(h.app().showing_start_screen());
    h.app_mut().take_start_choice(StartChoice::Template(budget));
    h.steps(2);

    assert!(
        !h.app().showing_start_screen(),
        "choosing a template left the start screen up"
    );
    // Planned total = 1200 + 400 + 120. A template whose formula was stored
    // as text would give a string here, and one that seeded nothing would
    // give an empty cell — both fail this.
    assert_eq!(
        h.app().display(CellRef::new(3, 1)),
        "1720",
        "the template's SUM did not evaluate"
    );
    // And a per-row formula: actual 1200 - planned 1200 = 0.
    assert_eq!(h.app().display(CellRef::new(0, 3)), "0");
    // A row whose actual is still 0: 0 - 400 = -400. Pins that the formula
    // reads its OWN row rather than a fixed one.
    assert_eq!(h.app().display(CellRef::new(1, 3)), "-400");
    // The formula is stored AS a formula, not baked into a literal.
    assert_eq!(h.app().edit_text(CellRef::new(3, 1)), "=SUM(B1:B3)");

    // A template workbook has no file behind it, so it must not have adopted
    // the previous file's identity.
    assert!(h.app().source_path().is_none());
}

/// A blank workbook from the start screen is empty and file-less.
#[test]
fn a_blank_workbook_starts_empty_and_owns_no_file() {
    use crate::harness::Harness;
    let _lock = crate::prefs::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = ConfigDirGuard::new("blank");

    let mut h = Harness::new(None);
    assert!(h.app().showing_start_screen());
    h.app_mut().take_start_choice(StartChoice::Blank);
    h.steps(2);

    assert!(!h.app().showing_start_screen());
    assert!(h.app().source_path().is_none());
    assert_eq!(h.app().row_count(), 0, "a blank workbook had rows in it");
}
