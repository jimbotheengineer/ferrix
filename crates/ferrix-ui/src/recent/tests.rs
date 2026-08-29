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
        !list.iter().any(|e| e.path == PathBuf::from("/f0.csv")),
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
