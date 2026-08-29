//! Tests for the protection model.
//!
//! Every assertion here is written to fail if the feature did nothing: the
//! lock tests read back the state of specific cells, the hash tests compare
//! against values the algorithm is pinned to, and the scale test asserts an
//! entry COUNT that a per-cell implementation could not possibly meet.

use super::*;

fn r(fr: u32, fc: u32, lr: u32, lc: u32) -> TableRange {
    TableRange::new(fr, fc, lr, lc)
}

fn c(row: u32, col: u32) -> CellRef {
    CellRef::new(row, col)
}

// ------------------------------------------------------------- lock map --

#[test]
fn cells_default_to_locked() {
    let p = SheetProtection::new();
    // The default is the whole point: an empty LockMap means everything is
    // locked, not everything is unlocked.
    assert!(p.is_locked(c(0, 0)));
    assert!(p.is_locked(c(199_999_999, 16_000)));
    assert!(p.unlocked().is_empty());
}

#[test]
fn unlocking_a_range_unlocks_exactly_that_range() {
    let mut m = LockMap::new();
    m.unlock(r(10, 2, 20, 4));
    assert!(m.is_unlocked(c(10, 2)), "top-left corner");
    assert!(m.is_unlocked(c(20, 4)), "bottom-right corner");
    assert!(m.is_unlocked(c(15, 3)), "interior");
    assert!(!m.is_unlocked(c(9, 3)), "row above must stay locked");
    assert!(!m.is_unlocked(c(21, 3)), "row below must stay locked");
    assert!(!m.is_unlocked(c(15, 1)), "column left must stay locked");
    assert!(!m.is_unlocked(c(15, 5)), "column right must stay locked");
}

#[test]
fn locking_punches_a_hole_and_keeps_the_rest_unlocked() {
    let mut m = LockMap::new();
    m.unlock(r(0, 0, 10, 10));
    m.lock(r(4, 4, 6, 6));
    // The hole is locked ...
    assert!(!m.is_unlocked(c(5, 5)));
    assert!(!m.is_unlocked(c(4, 4)));
    assert!(!m.is_unlocked(c(6, 6)));
    // ... and every side of it is not.
    assert!(m.is_unlocked(c(3, 5)), "above the hole");
    assert!(m.is_unlocked(c(7, 5)), "below the hole");
    assert!(m.is_unlocked(c(5, 3)), "left of the hole");
    assert!(m.is_unlocked(c(5, 7)), "right of the hole");
    assert!(
        m.is_unlocked(c(0, 0)) && m.is_unlocked(c(10, 10)),
        "corners"
    );
    // Four pieces, not 121 cells.
    assert!(m.len() <= 4, "hole split into {} pieces", m.len());
}

#[test]
fn relocking_the_whole_unlocked_range_empties_the_map() {
    let mut m = LockMap::new();
    m.unlock(r(2, 2, 8, 8));
    m.lock(r(0, 0, 100, 100));
    assert!(m.is_empty(), "nothing should be left unlocked");
    assert!(!m.is_unlocked(c(5, 5)));
}

#[test]
fn unlocking_twice_does_not_grow_the_map() {
    // A naive "just push it" implementation grows without bound when a user
    // drags over the same block repeatedly.
    let mut m = LockMap::new();
    for _ in 0..50 {
        m.unlock(r(0, 0, 5, 5));
    }
    assert_eq!(m.len(), 1, "overlapping unlocks must be normalised");
    assert!(m.is_unlocked(c(3, 3)));
}

#[test]
fn overlapping_unlocks_stay_disjoint_and_complete() {
    let mut m = LockMap::new();
    m.unlock(r(0, 0, 5, 5));
    m.unlock(r(3, 3, 8, 8));
    // Union coverage.
    for (row, col) in [(0, 0), (5, 5), (4, 4), (8, 8), (3, 7), (7, 3)] {
        assert!(
            m.is_unlocked(c(row, col)),
            "({row},{col}) should be unlocked"
        );
    }
    assert!(!m.is_unlocked(c(0, 6)));
    assert!(!m.is_unlocked(c(6, 0)));
    // Disjointness: no cell is covered twice.
    let covered = m.ranges().filter(|rg| rg.contains(c(4, 4))).count();
    assert_eq!(covered, 1, "stored rectangles must not overlap");
}

#[test]
fn deep_rows_are_addressed_exactly() {
    // The lookup window is bounded by the tallest entry; a short entry far
    // down the sheet must still be found.
    let mut m = LockMap::new();
    m.unlock(r(150_000_000, 3, 150_000_000, 3));
    assert!(m.is_unlocked(c(150_000_000, 3)));
    assert!(!m.is_unlocked(c(150_000_001, 3)));
    assert!(!m.is_unlocked(c(149_999_999, 3)));
}

#[test]
fn protecting_a_200m_row_column_costs_one_entry() {
    // THE scale invariant. A per-cell representation of the same statement is
    // 200 million entries; this asserts a number that only a per-range
    // implementation can hit.
    let mut p = SheetProtection::new();
    p.unlock_range(r(0, 0, 199_999_999, 0));
    p.lock_range(r(1000, 0, 1000, 0)); // one cell back to locked
    assert!(
        p.unlocked().len() <= 4,
        "{} entries for a 200M-row column with one hole",
        p.unlocked().len()
    );
    assert!(
        p.heap_bytes() < 4096,
        "{} bytes to describe 200M rows",
        p.heap_bytes()
    );
    assert!(!p.is_locked(c(199_999_999, 0)));
    assert!(p.is_locked(c(1000, 0)));
    assert!(
        p.is_locked(c(500, 1)),
        "the neighbouring column is untouched"
    );
}

// ------------------------------------------------------------ the states --

#[test]
fn a_lock_does_nothing_until_the_sheet_is_protected() {
    // The criterion says this "trips people up", so it is pinned: the same
    // cell reports two different states purely on the protection flag.
    let mut p = SheetProtection::new();
    let cell = c(2, 2);
    assert_eq!(p.state_of(cell), CellLockState::LockedButSheetUnprotected);
    assert_eq!(p.deny_edit(cell), None, "unprotected sheets refuse nothing");

    p.protect(Allowances::default(), PasswordHash::NONE);
    assert_eq!(p.state_of(cell), CellLockState::LockedAndEnforced);
    assert_eq!(p.deny_edit(cell), Some(Denied::LockedCell(cell)));

    p.unlock_range(r(2, 2, 2, 2));
    assert_eq!(p.state_of(cell), CellLockState::Unlocked);
    assert_eq!(p.deny_edit(cell), None);
}

#[test]
fn unprotecting_keeps_the_lock_flags() {
    let mut p = SheetProtection::new();
    p.unlock_range(r(0, 0, 1, 1));
    p.protect(Allowances::default(), PasswordHash::of("hunter2"));
    p.unprotect();
    assert!(!p.is_enabled());
    assert!(
        !p.is_locked(c(0, 0)),
        "unprotecting must not silently relock the sheet's unlocked cells"
    );
    assert!(p.hash().is_none(), "the password goes with the protection");
}

#[test]
fn allowances_gate_exactly_the_named_actions() {
    let mut p = SheetProtection::new();
    let allow = Allowances {
        sort: true,
        insert_rows: true,
        ..Allowances::default()
    };
    p.protect(allow, PasswordHash::NONE);

    assert_eq!(p.deny_action(Action::Sort), None);
    assert_eq!(p.deny_action(Action::InsertRows), None);
    assert_eq!(
        p.deny_action(Action::FormatCells),
        Some(Denied::SheetAction(Action::FormatCells))
    );
    assert_eq!(
        p.deny_action(Action::Filter),
        Some(Denied::SheetAction(Action::Filter))
    );
    assert_eq!(
        p.deny_action(Action::InsertColumns),
        Some(Denied::SheetAction(Action::InsertColumns))
    );
}

#[test]
fn an_unprotected_sheet_allows_every_action() {
    let p = SheetProtection::new();
    for a in [
        Action::FormatCells,
        Action::InsertRows,
        Action::InsertColumns,
        Action::DeleteRows,
        Action::DeleteColumns,
        Action::Sort,
        Action::Filter,
        Action::SelectLocked,
    ] {
        assert_eq!(p.deny_action(a), None, "{a:?} on an unprotected sheet");
    }
}

#[test]
fn every_refusal_explains_itself() {
    // "Editing a protected cell explains why rather than doing nothing."
    // A refusal whose message is empty, or which does not name the thing that
    // was refused, fails this.
    let msgs = [
        Denied::LockedCell(c(4, 1)).to_string(),
        Denied::SheetAction(Action::Sort).to_string(),
        Denied::Structure(StructureOp::RenameSheet).to_string(),
    ];
    assert!(msgs[0].contains("B5"), "must name the cell: {}", msgs[0]);
    assert!(msgs[0].contains("protected"));
    assert!(msgs[1].contains("sorting"), "{}", msgs[1]);
    assert!(msgs[2].contains("renaming a sheet"), "{}", msgs[2]);
    for m in &msgs {
        assert!(m.len() > 30, "a one-word refusal explains nothing: {m}");
    }
}

#[test]
fn the_locked_but_unprotected_state_says_so_in_words() {
    let words = CellLockState::LockedButSheetUnprotected.explain();
    assert!(
        words.contains("not protected") && words.contains("Protect the sheet"),
        "the confusing state must be spelled out: {words}"
    );
}

// ------------------------------------------------------ workbook structure --

#[test]
fn structure_protection_refuses_every_tab_operation() {
    let mut w = WorkbookProtection::new();
    for op in [
        StructureOp::AddSheet,
        StructureOp::DeleteSheet,
        StructureOp::RenameSheet,
        StructureOp::ReorderSheet,
    ] {
        assert_eq!(w.deny(op), None, "unprotected workbook allows {op:?}");
    }
    w.protect_structure(PasswordHash::of("x"));
    for op in [
        StructureOp::AddSheet,
        StructureOp::DeleteSheet,
        StructureOp::RenameSheet,
        StructureOp::ReorderSheet,
    ] {
        assert_eq!(w.deny(op), Some(Denied::Structure(op)));
    }
    assert!(w.is_active());
    w.unprotect();
    assert_eq!(w.deny(StructureOp::AddSheet), None);
    assert!(!w.is_active());
}

#[test]
fn windows_flag_survives_even_though_nothing_reads_it() {
    // Round-trip fidelity: dropping a flag we do not implement would silently
    // change the file.
    let w = WorkbookProtection::from_parts(true, true, PasswordHash::from_raw(0xABCD));
    assert!(w.windows_locked());
    assert!(w.structure_locked());
    assert_eq!(w.hash().raw(), 0xABCD);
}

// ------------------------------------------------------------- passwords --

#[test]
fn password_hash_matches_the_ecma_algorithm() {
    // Pinned against values produced by the published algorithm, so a
    // "simplification" of the rotate cannot pass unnoticed.
    assert_eq!(PasswordHash::of("").raw(), 0);
    assert_eq!(PasswordHash::of("password").to_hex(), "83AF");
    assert_eq!(PasswordHash::of("abc").to_hex(), "CC1A");
    assert_eq!(PasswordHash::of("secret").to_hex(), "DAA7");
}

#[test]
fn hex_round_trips() {
    let h = PasswordHash::of("secret");
    assert_eq!(PasswordHash::from_hex(&h.to_hex()), Some(h));
    assert_eq!(
        PasswordHash::from_hex("ce4b").map(|p| p.raw()),
        Some(0xCE4B)
    );
    assert_eq!(PasswordHash::from_hex("zzz"), None);
}

#[test]
fn verify_accepts_the_password_it_was_made_from() {
    let h = PasswordHash::of("Tr0ub4dor&3");
    assert!(h.verify("Tr0ub4dor&3"));
    assert!(!h.verify("Tr0ub4dor&4"));
}

#[test]
fn a_matching_secret_exists_for_every_reachable_hash() {
    // This is the round-trip enabler AND the honesty demonstration: for every
    // hash a file can legally contain, a nine-character string with the same
    // hash is derivable instantly. Exhaustive over the entire key space,
    // because the entire key space is 32768 values wide.
    let mut checked = 0usize;
    for v in 0u32..=0xFFFF {
        let h = PasswordHash::from_raw(v as u16);
        match h.matching_secret() {
            Some(s) => {
                assert_eq!(
                    PasswordHash::of(&s),
                    h,
                    "secret {s:?} does not hash back to {}",
                    h.to_hex()
                );
                checked += 1;
            }
            None => {
                // Only "no password" and the unreachable half of the space.
                assert!(
                    v == 0 || (v as u16 ^ (SECRET_LEN as u16) ^ HASH_SALT) & 0x8000 != 0,
                    "{v:04X} should have had a secret"
                );
            }
        }
    }
    assert_eq!(
        checked, 32768,
        "half the 16-bit space is reachable; got {checked}"
    );
}

#[test]
fn a_real_password_and_its_manufactured_twin_are_indistinguishable() {
    // Stated as a test so nobody has to take the module docs on faith: the
    // file cannot tell the user's password from one derived from the file.
    let real = PasswordHash::of("correct horse battery staple");
    let forged = real.matching_secret().expect("reachable hash");
    assert_ne!(forged, "correct horse battery staple");
    assert_eq!(PasswordHash::of(&forged), real);
    assert!(
        real.verify(&forged),
        "verify() cannot distinguish them, which is the point"
    );
}

#[test]
fn rotate_helpers_are_inverses() {
    for v in 0u32..0x8000 {
        let v = v as u16;
        assert_eq!(rotr15(rotl15(v)), v, "rotate round trip at {v:#06X}");
    }
}
