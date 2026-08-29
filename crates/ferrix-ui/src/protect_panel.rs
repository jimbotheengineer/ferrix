//! The Protect Sheet / Protect Workbook dialogs (issue #42).
//!
//! # The dialog's job is to tell the truth
//!
//! Excel's own Protect Sheet dialog says "Password to unprotect sheet" and
//! nothing else, and users reasonably conclude their data is protected. It is
//! not: the password becomes a sixteen-bit hash written into the file as plain
//! hex, and every value stays in the clear. See `ferrix_core::protect` for the
//! nine-character-collision demonstration.
//!
//! So [`DETERRENT_NOTICE`] is not optional decoration — it is one of the
//! acceptance criteria, and `dialog_states_plainly_that_this_is_not_security`
//! in `harness.rs` fails if it is removed or softened.
//!
//! # State only
//!
//! This module holds the dialog's *state* and the strings it draws. The
//! drawing itself lives in `app.rs` beside the other windows, and the actual
//! protecting goes through `Workbook::protection_mut` — so a test can drive
//! the same transitions the buttons drive without synthesising a click.

use ferrix_core::{Allowances, PasswordHash};

/// The sentence the dialog must show. Shown in full, never truncated, never
/// behind a disclosure triangle.
pub const DETERRENT_NOTICE: &str = "This is not security. Sheet protection deters accidents — \
     it stops a stray keystroke landing in a formula. It does not resist \
     anyone who wants in: nothing is encrypted, the values stay readable in \
     the file, and the password is stored as a 16-bit hash that takes \
     microseconds to work around. Do not use it to keep a secret.";

/// Shorter form for the status line, where the full notice will not fit.
pub const DETERRENT_SHORT: &str = "deters accidents, not attackers — nothing here is encrypted";

/// Which dialog is on screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProtectTarget {
    Sheet,
    Workbook,
}

/// The Protect dialog's live state.
#[derive(Clone, Debug)]
pub struct ProtectDialog {
    pub target: ProtectTarget,
    /// What the user typed. Never stored anywhere: it is hashed on Apply and
    /// this buffer is dropped. There is nowhere to store it that would be any
    /// safer, and keeping it would imply the password means something.
    pub password: String,
    pub allow: Allowances,
    /// Set when the user is *un*protecting a sheet that carries a password,
    /// so the dialog can ask for it. See [`ProtectDialog::unlock_matches`] for
    /// how little that check is worth.
    pub unprotecting: bool,
    /// The most recent message shown inside the dialog.
    pub message: Option<String>,
}

impl ProtectDialog {
    /// A dialog for protecting the active sheet, seeded from its current
    /// allowances so reopening it shows what is actually in force.
    pub fn for_sheet(allow: Allowances) -> Self {
        Self {
            target: ProtectTarget::Sheet,
            password: String::new(),
            allow,
            unprotecting: false,
            message: None,
        }
    }

    pub fn for_workbook() -> Self {
        Self {
            target: ProtectTarget::Workbook,
            password: String::new(),
            allow: Allowances::default(),
            unprotecting: false,
            message: None,
        }
    }

    /// A dialog asking for the password before unprotecting.
    pub fn unprotect(target: ProtectTarget) -> Self {
        Self {
            target,
            password: String::new(),
            allow: Allowances::default(),
            unprotecting: true,
            message: None,
        }
    }

    pub fn title(&self) -> &'static str {
        match (self.target, self.unprotecting) {
            (ProtectTarget::Sheet, false) => "Protect Sheet",
            (ProtectTarget::Sheet, true) => "Unprotect Sheet",
            (ProtectTarget::Workbook, false) => "Protect Workbook Structure",
            (ProtectTarget::Workbook, true) => "Unprotect Workbook",
        }
    }

    /// The hash of what is currently typed.
    pub fn hash(&self) -> PasswordHash {
        PasswordHash::of(&self.password)
    }

    /// Does the typed password match `expected`?
    ///
    /// A courtesy check, and it is labelled as one in the dialog. With a
    /// sixteen-bit hash, plenty of wrong passwords pass; more to the point,
    /// anyone who does not want to type one can edit the file instead. Ferrix
    /// asks because refusing to ask would surprise people coming from Excel,
    /// not because the answer proves anything.
    pub fn unlock_matches(&self, expected: PasswordHash) -> bool {
        expected.is_none() || expected.verify(&self.password)
    }

    /// The allowance checkboxes, in dialog order: label, and a closure pair to
    /// read and write the flag. Kept as one table so the dialog and any test
    /// enumerate the same set.
    pub fn allowance_rows(&mut self) -> Vec<(&'static str, &mut bool)> {
        let Allowances {
            select_locked_cells,
            select_unlocked_cells,
            format_cells,
            insert_rows,
            insert_columns,
            delete_rows,
            delete_columns,
            sort,
            use_autofilter,
        } = &mut self.allow;
        vec![
            ("Select locked cells", select_locked_cells),
            ("Select unlocked cells", select_unlocked_cells),
            ("Format cells", format_cells),
            ("Insert rows", insert_rows),
            ("Insert columns", insert_columns),
            ("Delete rows", delete_rows),
            ("Delete columns", delete_columns),
            ("Sort", sort),
            ("Use AutoFilter", use_autofilter),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_says_the_three_things_that_matter() {
        // Each clause is asserted separately, so softening any one of them
        // fails rather than being absorbed by the length of the rest.
        let n = DETERRENT_NOTICE;
        assert!(n.contains("not security"), "must deny being security: {n}");
        assert!(
            n.contains("nothing is encrypted"),
            "must deny encryption: {n}"
        );
        assert!(
            n.contains("16-bit hash"),
            "must say how weak the password is: {n}"
        );
        assert!(
            n.to_lowercase().contains("accident"),
            "must say what it IS for: {n}"
        );
    }

    #[test]
    fn every_allowance_field_gets_a_row() {
        // A checkbox missing from the dialog is an allowance the user cannot
        // reach, which is how "granular allowances" quietly becomes "one
        // switch". Nine fields, nine rows.
        let mut d = ProtectDialog::for_sheet(Allowances::default());
        assert_eq!(d.allowance_rows().len(), 9);
    }

    #[test]
    fn the_rows_write_through_to_the_allowances() {
        let mut d = ProtectDialog::for_sheet(Allowances::default());
        assert!(!d.allow.sort);
        for (label, flag) in d.allowance_rows() {
            if label == "Sort" {
                *flag = true;
            }
        }
        assert!(d.allow.sort, "toggling a row must reach the model");
    }

    #[test]
    fn an_unpassworded_sheet_unlocks_without_one() {
        let d = ProtectDialog::unprotect(ProtectTarget::Sheet);
        assert!(d.unlock_matches(PasswordHash::NONE));
    }

    #[test]
    fn a_wrong_password_is_refused_and_the_right_one_is_not() {
        let mut d = ProtectDialog::unprotect(ProtectTarget::Sheet);
        let expected = PasswordHash::of("opensesame");
        d.password = "nope".into();
        assert!(!d.unlock_matches(expected));
        d.password = "opensesame".into();
        assert!(d.unlock_matches(expected));
    }

    #[test]
    fn titles_distinguish_all_four_states() {
        let mut seen = std::collections::HashSet::new();
        for target in [ProtectTarget::Sheet, ProtectTarget::Workbook] {
            for unprotecting in [false, true] {
                let mut d = ProtectDialog::for_sheet(Allowances::default());
                d.target = target;
                d.unprotecting = unprotecting;
                assert!(seen.insert(d.title()), "duplicate title {}", d.title());
            }
        }
        assert_eq!(seen.len(), 4);
    }
}
