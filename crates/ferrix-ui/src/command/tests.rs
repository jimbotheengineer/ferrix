//! Tests for the command registry and palette model.
//!
//! The frame-level behaviour — the open chord, Escape restoring the selection,
//! an edit surviving the palette — lives in `harness.rs`, which drives the real
//! `FerrixApp`. What is here is what can be decided without a frame: the
//! registry's own invariants, ranking, and availability reasons.

use super::*;

/// The state a freshly opened app with a loaded file is in: nothing running,
/// nothing frozen, nothing to undo.
fn quiet_state() -> CommandState {
    CommandState {
        zoom: 1.0,
        selection_label: "A1".into(),
        ..Default::default()
    }
}

#[test]
fn every_command_is_reachable_from_the_palette() {
    // The central acceptance criterion of issue #40, stated the only way that
    // can actually fail: the palette's unfiltered list must be the registry,
    // not a subset of it.
    let p = CommandPalette::default();
    let listed: Vec<CommandId> = p.matches(&quiet_state()).iter().map(|m| m.id).collect();
    for c in REGISTRY {
        assert!(
            listed.contains(&c.id),
            "{} is in the registry but the palette never lists it",
            c.slug
        );
    }
    assert_eq!(listed.len(), REGISTRY.len());
}

#[test]
fn every_menu_item_is_a_registry_command() {
    // The other half: a menu cannot contain anything the palette does not.
    // `menu_items` iterates `for_menu`, so this holds by construction — the
    // assertion is that the construction is still what it claims to be, i.e.
    // that every menu is non-empty and drawn from the same table.
    let p = CommandPalette::default();
    let listed: Vec<CommandId> = p.matches(&quiet_state()).iter().map(|m| m.id).collect();
    let mut total = 0;
    for menu in Menu::ALL {
        let items: Vec<&Command> = for_menu(menu).collect();
        assert!(
            !items.is_empty(),
            "{} draws no items — a menu built from a second list would look like this",
            menu.title()
        );
        for c in items {
            assert!(
                listed.contains(&c.id),
                "{} is in the {} menu but not in the palette",
                c.slug,
                menu.title()
            );
            total += 1;
        }
    }
    assert!(
        total >= 20,
        "only {total} menu commands — the menu bar lost items in the refactor"
    );
}

#[test]
fn slugs_and_ids_are_unique() {
    // A duplicated slug silently merges two commands' recency entries.
    let mut slugs: Vec<&str> = REGISTRY.iter().map(|c| c.slug).collect();
    slugs.sort_unstable();
    let n = slugs.len();
    slugs.dedup();
    assert_eq!(slugs.len(), n, "duplicate command slug");

    let mut ids: Vec<String> = REGISTRY.iter().map(|c| format!("{:?}", c.id)).collect();
    ids.sort();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate CommandId");
}

#[test]
fn fuzzy_search_finds_by_subsequence_and_rejects_non_matches() {
    let p = CommandPalette {
        query: "goal".into(),
        ..Default::default()
    };
    let ids: Vec<CommandId> = p.matches(&quiet_state()).iter().map(|m| m.id).collect();
    assert_eq!(
        ids,
        vec![CommandId::DataGoalSeek],
        "a precise query must filter down to the precise command"
    );

    // Non-contiguous subsequence: "cndf" -> "Conditional Formatting".
    let p = CommandPalette {
        query: "cndf".into(),
        ..Default::default()
    };
    let ids: Vec<CommandId> = p.matches(&quiet_state()).iter().map(|m| m.id).collect();
    assert!(
        ids.contains(&CommandId::FormatCondNew),
        "fuzzy search missed Conditional Formatting for 'cndf'; got {ids:?}"
    );

    // And a query that matches nothing must return nothing, or the filter is
    // doing nothing at all.
    let p = CommandPalette {
        query: "zzzqqq".into(),
        ..Default::default()
    };
    assert!(p.matches(&quiet_state()).is_empty());
}

#[test]
fn shortcuts_are_shown_for_the_commands_that_have_them() {
    let p = CommandPalette::default();
    let list = p.matches(&quiet_state());
    let save = list
        .iter()
        .find(|m| m.id == CommandId::FileSave)
        .expect("Save is in the registry");
    assert_eq!(save.shortcut, Some("Ctrl+S"));
    let zoom = list
        .iter()
        .find(|m| m.id == CommandId::ViewZoomReset)
        .expect("Reset zoom is in the registry");
    assert_eq!(zoom.shortcut, Some("Ctrl+0"));
    // Some commands genuinely have none; the field must distinguish that from
    // an empty string rendered as a blank column.
    assert!(list.iter().any(|m| m.shortcut.is_none()));
}

#[test]
fn recently_used_commands_rank_first() {
    let mut p = CommandPalette::default();
    let st = quiet_state();

    // Two commands both matching "z": zoom in, zoom out, reset zoom.
    let before: Vec<CommandId> = {
        p.query = "zoom".into();
        p.matches(&st).iter().map(|m| m.id).collect()
    };
    assert!(before.len() >= 3, "expected several zoom commands");

    // Run the LAST of them; it must move to the front of the equal-score set.
    let last = *before.last().unwrap();
    p.record(last);
    let after: Vec<CommandId> = p.matches(&st).iter().map(|m| m.id).collect();
    assert_eq!(
        after[0], last,
        "the most recently run command must rank first: {before:?} -> {after:?}"
    );
    assert_ne!(
        before, after,
        "recording a command changed nothing about the ranking"
    );

    // A newer run displaces the older one.
    let other = before[0];
    p.record(other);
    let after2: Vec<CommandId> = p.matches(&st).iter().map(|m| m.id).collect();
    assert_eq!(after2[0], other);
    assert_eq!(after2[1], last, "the previous winner must fall to second");
}

#[test]
fn recency_never_beats_a_better_score() {
    // Otherwise the palette becomes unusable: typing the exact name of a
    // command would still surface whatever was run last.
    let mut p = CommandPalette::default();
    p.record(CommandId::FileOpen);
    p.query = "goal seek".into();
    let ids: Vec<CommandId> = p.matches(&quiet_state()).iter().map(|m| m.id).collect();
    assert_eq!(ids.first(), Some(&CommandId::DataGoalSeek));
}

#[test]
fn recency_round_trips_through_slugs_and_drops_unknown_ones() {
    let mut p = CommandPalette::default();
    p.record(CommandId::FileSave);
    p.record(CommandId::DataGoalSeek);
    let slugs = p.recent_slugs();
    assert_eq!(slugs, vec!["data.goal_seek", "file.save"]);

    let mut restored = CommandPalette::default();
    restored.set_recent_slugs(&slugs);
    assert_eq!(
        restored.recent(),
        &[CommandId::DataGoalSeek, CommandId::FileSave],
        "a restart must restore the exact ranking, in order"
    );

    // A slug from a build that had a command this one does not.
    let mut older = CommandPalette::default();
    older.set_recent_slugs(&[
        "file.save".into(),
        "command.that.no.longer.exists".into(),
        "data.goal_seek".into(),
    ]);
    assert_eq!(
        older.recent(),
        &[CommandId::FileSave, CommandId::DataGoalSeek],
        "an unknown slug must be skipped, not panic and not shift the rest"
    );
}

#[test]
fn recency_is_bounded() {
    let mut p = CommandPalette::default();
    for _ in 0..4 {
        for c in REGISTRY {
            p.record(c.id);
        }
    }
    assert!(p.recent().len() <= MAX_RECENT);
    // No duplicates: recording an existing entry moves it, never appends.
    let mut seen = p.recent().to_vec();
    let n = seen.len();
    seen.sort_by_key(|id| format!("{id:?}"));
    seen.dedup();
    assert_eq!(seen.len(), n, "the recency list accumulated duplicates");
}

#[test]
fn unavailable_commands_are_listed_disabled_with_a_reason() {
    // The criterion is specifically that they are NOT hidden: a user who
    // searches "compact" and finds nothing concludes the feature is missing.
    let st = CommandState {
        can_compact: false,
        compact_hint: "Nothing to compact — there are no edits to bake in".into(),
        zoom: 1.0,
        ..Default::default()
    };
    let p = CommandPalette {
        query: "compact".into(),
        ..Default::default()
    };
    let list = p.matches(&st);
    let m = list
        .iter()
        .find(|m| m.id == CommandId::FileCompact)
        .expect("Compact must still be LISTED when it cannot run");
    assert_eq!(
        m.disabled.as_deref(),
        Some("Nothing to compact — there are no edits to bake in"),
        "a disabled command must carry the reason, not just a grey flag"
    );

    // And when it can run, no reason and no greying.
    let st = CommandState {
        can_compact: true,
        zoom: 1.0,
        ..Default::default()
    };
    let list = p.matches(&st);
    let m = list
        .iter()
        .find(|m| m.id == CommandId::FileCompact)
        .unwrap();
    assert_eq!(m.disabled, None);
}

#[test]
fn every_disabled_reason_is_a_sentence_not_a_flag() {
    // Guards the failure mode the issue calls out: a greyed item with an empty
    // or placeholder explanation is no better than hiding it.
    let st = CommandState::default();
    for c in REGISTRY {
        if let Some(r) = c.disabled_reason(&st) {
            assert!(
                r.len() > 8 && r.contains(' '),
                "{} has a useless disabled reason: {r:?}",
                c.slug
            );
        }
    }
}

#[test]
fn availability_tracks_the_state_it_claims_to() {
    let mut st = quiet_state();
    let undo = Command::of(CommandId::EditUndo);
    assert!(undo.disabled_reason(&st).is_some(), "nothing to undo yet");
    st.can_undo = true;
    assert!(undo.disabled_reason(&st).is_none(), "undo became available");

    let unfreeze = Command::of(CommandId::ViewUnfreeze);
    assert!(unfreeze.disabled_reason(&st).is_some());
    st.frozen = true;
    assert!(unfreeze.disabled_reason(&st).is_none());

    let clear = Command::of(CommandId::FormulaTraceClear);
    assert!(clear.disabled_reason(&st).is_some());
    st.has_trace = true;
    assert!(clear.disabled_reason(&st).is_none());

    // Zoom bounds: the reason must appear only AT the stop.
    let zin = Command::of(CommandId::ViewZoomIn);
    st.zoom = 1.0;
    assert!(zin.disabled_reason(&st).is_none());
    st.zoom = crate::grid::MAX_ZOOM;
    assert!(zin.disabled_reason(&st).is_some());
}

#[test]
fn menu_labels_carry_the_shortcut() {
    assert_eq!(
        Command::of(CommandId::FileSave).menu_label(),
        "💾 Save edits  (Ctrl+S)"
    );
    assert_eq!(
        Command::of(CommandId::FileOpen).menu_label(),
        "📂 Open CSV…",
        "a command with no shortcut must not gain empty parentheses"
    );
}

#[test]
fn close_restores_only_when_asked() {
    use ferrix_core::CellRef;
    let sel = Selection::new(CellRef::new(3, 4), CellRef::new(9, 9));
    let mut p = CommandPalette::default();
    p.open(sel);
    assert!(p.is_open());
    // Escape: give the selection back exactly.
    let restored = p.close(true).expect("Escape must restore the selection");
    assert_eq!(restored.anchor, CellRef::new(3, 4));
    assert_eq!(restored.cursor, CellRef::new(9, 9));
    assert!(!p.is_open());

    // Enter: a command may have MOVED the cursor deliberately (undo, goal
    // seek), so running must not put the old selection back.
    p.open(sel);
    assert!(p.close(false).is_none());
}

#[test]
fn opening_resets_the_query_and_the_cursor() {
    use ferrix_core::CellRef;
    let mut p = CommandPalette::default();
    p.open(Selection::single(CellRef::new(0, 0)));
    p.query = "zoom".into();
    p.cursor = 3;
    p.close(true);
    p.open(Selection::single(CellRef::new(0, 0)));
    assert_eq!(p.query, "", "a stale query would hide most commands");
    assert_eq!(p.cursor, 0);
}

#[test]
fn an_empty_query_matches_everything_at_equal_score() {
    assert_eq!(fuzzy_score("", "anything"), Some(0));
    assert_eq!(fuzzy_score("   ", "anything"), Some(0));
    // Case insensitivity in both directions.
    assert!(fuzzy_score("ZOOM", "＋ Zoom in").is_some());
    assert!(fuzzy_score("zoom", "＋ ZOOM IN").is_some());
    // Out-of-order characters are not a subsequence.
    assert_eq!(fuzzy_score("moz", "Zoom"), None);
}
