//! Workbook and sheet protection (issue #42).
//!
//! # This is not security. Read this paragraph before using anything here.
//!
//! Excel's sheet and workbook protection is a *user-interface convention*, not
//! a confidentiality control. Nothing in this module encrypts, signs or hides
//! anything: the cell values are stored in the clear, the "password" is
//! reduced to a **16-bit** hash that is written into the file in plain hex,
//! and every protection flag is a boolean that any reader is free to ignore.
//!
//! To make the point unmissable rather than a footnote,
//! [`PasswordHash::matching_secret`] manufactures a string that hashes to any
//! given value in constant time. That function exists because the round trip
//! needs it, but it is also the honest demonstration: if a nine-character
//! string equivalent to the user's password can be derived from the file in
//! microseconds, the password was never protecting anything.
//!
//! What protection *is* good for is the thing it is actually used for: saying
//! "don't type here" to a colleague who is trying to fill in the four yellow
//! cells on a form and not the two hundred formulas around them. It deters
//! accident. It does not resist attack, and the UI must say so — see
//! `crates/ferrix-io/src/edits.rs`, which has been making this point about a
//! neighbouring feature for a while: the user believes they are protected
//! right up until they try to use it.
//!
//! # Storage: per range, never per cell
//!
//! Cells default to **locked**, and the flag only bites once the sheet is
//! protected — which trips everyone up, so [`SheetProtection::state_of`]
//! reports the two facts separately rather than collapsing them to one bool.
//!
//! Because the default is "locked", the sparse thing worth storing is the set
//! of *unlocked* rectangles, held in [`LockMap`] using the same shape as
//! [`crate::merge::MergeMap`]: a `BTreeMap` keyed by first row plus the height
//! of the tallest entry, so a lookup range-queries a bounded window instead of
//! scanning every rectangle.
//!
//! Protecting a 200M-row column is therefore **one entry**, and unlocking four
//! input cells inside it is four more. Nothing here is ever proportional to
//! the row count. [`LockMap::lock`] subtracts rectangles rather than
//! materialising cells, so the entry count tracks the number of *gestures* the
//! user made, not the area they covered.

use std::collections::BTreeMap;

use crate::table::TableRange;
use crate::CellRef;

/// The magic constant from ECMA-376-4 §18.2.29's password hash.
const HASH_SALT: u16 = 0xCE4B;

/// Length of the string [`PasswordHash::matching_secret`] builds.
///
/// Nine is the minimum that works: each byte contributes at most seven usable
/// bits (we keep every byte ASCII), the hash rotates left one bit per byte,
/// and reaching bit 14 of a 15-bit accumulator therefore needs nine rounds.
const SECRET_LEN: usize = 9;

/// Excel's password hash: sixteen bits, stored in the file as plain hex.
///
/// `0` means "protected without a password", which is also Excel's spelling
/// for it — the `password` attribute is simply absent.
///
/// # Why this type exists rather than a `String`
///
/// A file never contains the password, only this hash, so an importer cannot
/// recover what the user typed and an exporter has nothing to re-hash. Keeping
/// the *hash* as the model means an imported workbook can be written back out
/// with its original hash intact, which is the round-trip criterion; keeping a
/// `String` would have silently dropped it.
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct PasswordHash(u16);

impl std::fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PasswordHash({})", self.to_hex())
    }
}

impl PasswordHash {
    /// No password at all.
    pub const NONE: PasswordHash = PasswordHash(0);

    /// Hash a password with Excel's algorithm (ECMA-376-4 §18.2.29).
    ///
    /// Sixteen bits. Roughly sixty-five thousand possible values for any
    /// password of any length, which is why this is not security.
    pub fn of(password: &str) -> Self {
        if password.is_empty() {
            return Self::NONE;
        }
        let mut hash: u16 = 0;
        for byte in password.as_bytes().iter().rev() {
            hash = rotl15(hash) ^ u16::from(*byte);
        }
        hash = rotl15(hash);
        hash ^= password.len() as u16;
        hash ^= HASH_SALT;
        Self(hash)
    }

    /// Wrap a hash read straight out of a file.
    pub fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    pub fn raw(self) -> u16 {
        self.0
    }

    /// Is this "no password"?
    pub fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Does `password` hash to this value?
    ///
    /// **A `true` here means very little.** The hash is 16 bits, so an average
    /// of 2^(8n)/65536 strings of length n collide with any given value. This
    /// answers "did they probably type the same thing", not "are they
    /// authorised".
    pub fn verify(self, password: &str) -> bool {
        Self::of(password) == self
    }

    /// The four-hex-digit spelling the OOXML `password` attribute uses.
    pub fn to_hex(self) -> String {
        format!("{:04X}", self.0)
    }

    /// Parse a `password="A1B2"` attribute.
    pub fn from_hex(s: &str) -> Option<Self> {
        u16::from_str_radix(s.trim(), 16).ok().map(Self)
    }

    /// A string that hashes to exactly this value.
    ///
    /// # Why this is needed
    ///
    /// Writers — ours included, via `rust_xlsxwriter` — take a *password* and
    /// hash it. An imported file gives us only the hash. Re-exporting would
    /// therefore have to either drop the password (the file silently loses its
    /// protection, which the acceptance criteria forbid) or supply a string
    /// that hashes to the same thing. This is that string.
    ///
    /// # Why it is trivial
    ///
    /// The hash is an affine map over GF(2): each byte is XORed into a 15-bit
    /// accumulator that rotates one bit per round. Inverting it is a few
    /// shifts, so this runs in constant time with no search at all. Nine
    /// printable-ASCII-or-NUL bytes suffice for every reachable hash.
    ///
    /// **Treat the existence of this function as the specification of how much
    /// an Excel sheet password is worth.** It is not a weakness in this
    /// implementation; the file format cannot carry more than sixteen bits of
    /// the password, so no implementation can do better.
    ///
    /// Returns `None` for [`PasswordHash::NONE`] (there is nothing to match)
    /// and for the hash values no password can produce — bit 15 of the salt is
    /// set and the accumulator is only fifteen bits wide, so every real Excel
    /// hash has its top bit set, and a file claiming otherwise is malformed.
    pub fn matching_secret(self) -> Option<String> {
        if self.is_none() {
            return None;
        }
        // Undo the trailing `^ len ^ salt`. `x` must be a legal rotate output,
        // i.e. fifteen bits; anything else was never produced by this hash.
        let x = self.0 ^ (SECRET_LEN as u16) ^ HASH_SALT;
        if x & 0x8000 != 0 {
            return None;
        }
        // ... and the trailing rotate, leaving the accumulator after the byte
        // loop.
        let v = rotr15(x);

        // Solve for the bytes. Byte `i` (counting from the one consumed FIRST,
        // i.e. the last character of the string) lands at rotate offset
        // `SECRET_LEN - 1 - i`, so the first byte reaches the top of the
        // accumulator and the last reaches the bottom. Keeping every byte to
        // seven bits keeps the result ASCII, and therefore a legal `&str`.
        let mut bytes = [0u8; SECRET_LEN];
        let b0 = ((v >> 8) & 0x7F) as u8; // covers bits 8..=14
        let rest = v ^ (u16::from(b0) << 8);
        let b7 = ((rest >> 1) & 0x7F) as u8; // covers bits 1..=7
        let rest = rest ^ (u16::from(b7) << 1);
        let b8 = (rest & 0x01) as u8; // bit 0
        bytes[0] = b0;
        bytes[SECRET_LEN - 2] = b7;
        bytes[SECRET_LEN - 1] = b8;

        // `bytes` is in consumption order and the hash consumes the string
        // backwards, so the string is the reverse.
        bytes.reverse();
        // Every byte is <= 0x7F, so this is ASCII and cannot fail.
        String::from_utf8(bytes.to_vec()).ok()
    }
}

/// Rotate a 15-bit accumulator left by one.
#[inline]
fn rotl15(h: u16) -> u16 {
    ((h >> 14) & 0x01) | ((h << 1) & 0x7FFF)
}

/// The inverse of [`rotl15`], for inputs already reduced to fifteen bits.
#[inline]
fn rotr15(h: u16) -> u16 {
    ((h & 0x01) << 14) | ((h & 0x7FFF) >> 1)
}

// ============================================================== lock map ==

/// The rectangles of a sheet that are **unlocked**.
///
/// Cells default to locked, so the sparse set worth keeping is the exceptions.
/// Same storage shape as [`crate::merge::MergeMap`]: keyed by first row, with
/// the tallest entry's height bounding the lookup window.
///
/// Entries are kept pairwise disjoint by [`LockMap::unlock`], which subtracts
/// the incoming rectangle from the existing ones before pushing it. That means
/// unlocking the same block twice does not grow the map, and locking it again
/// returns the map to its previous size rather than leaving fragments behind.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct LockMap {
    by_row: BTreeMap<u32, Vec<TableRange>>,
    tallest: u32,
    len: usize,
}

impl LockMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of stored rectangles. **Not** a cell count: unlocking an entire
    /// 200M-row column is one.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn ranges(&self) -> impl Iterator<Item = &TableRange> {
        self.by_row.values().flatten()
    }

    pub fn heap_bytes(&self) -> usize {
        self.by_row
            .values()
            .map(|v| v.capacity() * std::mem::size_of::<TableRange>())
            .sum::<usize>()
            + self.by_row.len()
                * (std::mem::size_of::<u32>() + std::mem::size_of::<Vec<TableRange>>())
    }

    /// Is this cell unlocked?
    pub fn is_unlocked(&self, cell: CellRef) -> bool {
        if self.by_row.is_empty() {
            return false;
        }
        let lo = cell.row.saturating_sub(self.tallest.saturating_sub(1));
        self.by_row
            .range(lo..=cell.row)
            .flat_map(|(_, v)| v)
            .any(|r| r.contains(cell))
    }

    /// Mark a rectangle unlocked.
    pub fn unlock(&mut self, range: TableRange) {
        // Subtract first so the stored set stays disjoint and repeated
        // unlocking of overlapping areas cannot grow without bound.
        self.lock(range);
        self.push(range);
    }

    /// Mark a rectangle locked again, by subtracting it from the unlocked set.
    ///
    /// Splitting a rectangle around a hole yields at most four pieces, so the
    /// map's size is bounded by the number of gestures the user made — never
    /// by the area they covered.
    pub fn lock(&mut self, range: TableRange) {
        let hits: Vec<TableRange> = self
            .ranges()
            .filter(|r| intersects(**r, range))
            .copied()
            .collect();
        if hits.is_empty() {
            return;
        }
        for h in &hits {
            self.remove(*h);
        }
        for h in &hits {
            for piece in subtract(*h, range) {
                self.push(piece);
            }
        }
    }

    /// Forget every exception: the whole sheet is locked again.
    pub fn clear(&mut self) {
        self.by_row.clear();
        self.tallest = 0;
        self.len = 0;
    }

    fn push(&mut self, range: TableRange) {
        let height = range.last_row - range.first_row + 1;
        self.tallest = self.tallest.max(height);
        self.by_row.entry(range.first_row).or_default().push(range);
        self.len += 1;
    }

    fn remove(&mut self, range: TableRange) {
        if let Some(v) = self.by_row.get_mut(&range.first_row) {
            if let Some(pos) = v.iter().position(|r| *r == range) {
                v.remove(pos);
                self.len -= 1;
            }
            if v.is_empty() {
                self.by_row.remove(&range.first_row);
            }
        }
        // `tallest` is deliberately not recomputed, for the reason
        // `MergeMap::unmerge_at` gives: a stale-high value only widens the
        // lookup window, and recomputing would be O(n) per removal.
    }
}

fn intersects(a: TableRange, b: TableRange) -> bool {
    a.first_row <= b.last_row
        && b.first_row <= a.last_row
        && a.first_col <= b.last_col
        && b.first_col <= a.last_col
}

/// `a` minus `b`, as up to four disjoint rectangles.
fn subtract(a: TableRange, b: TableRange) -> Vec<TableRange> {
    if !intersects(a, b) {
        return vec![a];
    }
    let mut out = Vec::new();
    // Band above the hole.
    if a.first_row < b.first_row {
        out.push(TableRange::new(
            a.first_row,
            a.first_col,
            b.first_row - 1,
            a.last_col,
        ));
    }
    // Band below.
    if a.last_row > b.last_row {
        out.push(TableRange::new(
            b.last_row + 1,
            a.first_col,
            a.last_row,
            a.last_col,
        ));
    }
    // Left and right of the hole, only over the overlapping rows.
    let r0 = a.first_row.max(b.first_row);
    let r1 = a.last_row.min(b.last_row);
    if a.first_col < b.first_col {
        out.push(TableRange::new(r0, a.first_col, r1, b.first_col - 1));
    }
    if a.last_col > b.last_col {
        out.push(TableRange::new(r0, b.last_col + 1, r1, a.last_col));
    }
    out
}

// =========================================================== allowances ==

/// What a protected sheet still permits.
///
/// Field names and defaults follow Excel's Protect Sheet dialog, so a file
/// written elsewhere means here what it meant there. Everything defaults to
/// "not allowed" except selecting cells, which is what Excel does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Allowances {
    pub select_locked_cells: bool,
    pub select_unlocked_cells: bool,
    pub format_cells: bool,
    pub insert_rows: bool,
    pub insert_columns: bool,
    pub delete_rows: bool,
    pub delete_columns: bool,
    pub sort: bool,
    pub use_autofilter: bool,
}

impl Default for Allowances {
    fn default() -> Self {
        Self {
            select_locked_cells: true,
            select_unlocked_cells: true,
            format_cells: false,
            insert_rows: false,
            insert_columns: false,
            delete_rows: false,
            delete_columns: false,
            sort: false,
            use_autofilter: false,
        }
    }
}

/// An action a protected sheet or workbook can refuse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    FormatCells,
    InsertRows,
    InsertColumns,
    DeleteRows,
    DeleteColumns,
    Sort,
    Filter,
    SelectLocked,
}

impl Action {
    /// The name to put in a sentence: "sorting is turned off ...".
    pub fn gerund(self) -> &'static str {
        match self {
            Action::FormatCells => "formatting cells",
            Action::InsertRows => "inserting rows",
            Action::InsertColumns => "inserting columns",
            Action::DeleteRows => "deleting rows",
            Action::DeleteColumns => "deleting columns",
            Action::Sort => "sorting",
            Action::Filter => "filtering",
            Action::SelectLocked => "selecting locked cells",
        }
    }
}

/// A structural change to the workbook itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StructureOp {
    AddSheet,
    DeleteSheet,
    RenameSheet,
    ReorderSheet,
}

impl StructureOp {
    pub fn gerund(self) -> &'static str {
        match self {
            StructureOp::AddSheet => "adding a sheet",
            StructureOp::DeleteSheet => "deleting a sheet",
            StructureOp::RenameSheet => "renaming a sheet",
            StructureOp::ReorderSheet => "reordering the sheets",
        }
    }
}

/// Why something was refused, phrased so it can be shown verbatim.
///
/// The acceptance criteria ask that editing a protected cell **explain** rather
/// than do nothing. That is only achievable if the refusal carries the reason
/// all the way to the caller, so every guard in this module returns one of
/// these rather than a bare `false`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Denied {
    /// The cell is locked and the sheet is protected.
    LockedCell(CellRef),
    /// The sheet is protected and this allowance is off.
    SheetAction(Action),
    /// The workbook's structure is protected.
    Structure(StructureOp),
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Denied::LockedCell(c) => write!(
                f,
                "{} is locked and this sheet is protected — unprotect the sheet, \
                 or unlock this cell, to edit it",
                c.to_a1()
            ),
            Denied::SheetAction(a) => write!(
                f,
                "this sheet is protected and {} is not among the actions it allows",
                a.gerund()
            ),
            Denied::Structure(op) => write!(
                f,
                "the workbook's structure is protected, so {} is not allowed",
                op.gerund()
            ),
        }
    }
}

impl std::error::Error for Denied {}

/// What the UI should say about one cell.
///
/// Locked-and-unprotected is the state everybody trips over: the cell carries
/// the lock flag, the flag does nothing, and the user concludes locking is
/// broken. Keeping it a distinct variant is what lets the status bar say so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellLockState {
    /// Locked, but the sheet is not protected — so the flag has no effect yet.
    LockedButSheetUnprotected,
    /// Locked on a protected sheet: edits are refused.
    LockedAndEnforced,
    /// Explicitly unlocked; editable whether or not the sheet is protected.
    Unlocked,
}

impl CellLockState {
    /// One sentence for the status bar.
    pub fn explain(self) -> &'static str {
        match self {
            CellLockState::LockedButSheetUnprotected => {
                "Locked — but this sheet is not protected, so the lock does nothing yet. \
                 Protect the sheet to make it bite."
            }
            CellLockState::LockedAndEnforced => {
                "Locked, and this sheet is protected: edits here are refused."
            }
            CellLockState::Unlocked => "Unlocked — editable even while the sheet is protected.",
        }
    }
}

// ====================================================== sheet protection ==

/// One sheet's protection state.
///
/// Cheap to clone and bounded by the number of unlocked rectangles, so a
/// protected 200M-row sheet costs the same as a protected 10-row one.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SheetProtection {
    enabled: bool,
    hash: PasswordHash,
    allow: Allowances,
    unlocked: LockMap,
}

impl SheetProtection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is the sheet protected right now?
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn hash(&self) -> PasswordHash {
        self.hash
    }

    pub fn allow(&self) -> &Allowances {
        &self.allow
    }

    pub fn unlocked(&self) -> &LockMap {
        &self.unlocked
    }

    /// Turn protection on with a set of allowances and an optional password.
    pub fn protect(&mut self, allow: Allowances, hash: PasswordHash) {
        self.enabled = true;
        self.allow = allow;
        self.hash = hash;
    }

    /// Turn protection off. Keeps the lock flags — they are a property of the
    /// cells, not of the protection, and Excel keeps them too.
    ///
    /// Takes no password on purpose. Callers that want to make the user type
    /// one should ask [`PasswordHash::verify`] first and say plainly that a
    /// 16-bit check is a formality; pretending otherwise inside the model
    /// would be the "looks secure" failure this module exists to avoid.
    pub fn unprotect(&mut self) {
        self.enabled = false;
        self.hash = PasswordHash::NONE;
    }

    /// Mark a range unlocked (editable while protected).
    pub fn unlock_range(&mut self, range: TableRange) {
        self.unlocked.unlock(range);
    }

    /// Mark a range locked again (the default state).
    pub fn lock_range(&mut self, range: TableRange) {
        self.unlocked.lock(range);
    }

    /// Is this cell carrying the lock flag? Independent of whether the sheet
    /// is protected — that is exactly the distinction users miss.
    #[inline]
    pub fn is_locked(&self, cell: CellRef) -> bool {
        !self.unlocked.is_unlocked(cell)
    }

    /// The three-way state to report in the UI.
    pub fn state_of(&self, cell: CellRef) -> CellLockState {
        match (self.is_locked(cell), self.enabled) {
            (false, _) => CellLockState::Unlocked,
            (true, false) => CellLockState::LockedButSheetUnprotected,
            (true, true) => CellLockState::LockedAndEnforced,
        }
    }

    /// Why an edit to `cell` must be refused, or `None` to let it through.
    ///
    /// This is the predicate the edit chokepoint calls. It returns a reason
    /// rather than a bool so the refusal can be explained, which is one of the
    /// acceptance criteria.
    #[inline]
    pub fn deny_edit(&self, cell: CellRef) -> Option<Denied> {
        if self.enabled && self.is_locked(cell) {
            Some(Denied::LockedCell(cell))
        } else {
            None
        }
    }

    /// Why `action` must be refused, or `None`.
    pub fn deny_action(&self, action: Action) -> Option<Denied> {
        if !self.enabled {
            return None;
        }
        let allowed = match action {
            Action::FormatCells => self.allow.format_cells,
            Action::InsertRows => self.allow.insert_rows,
            Action::InsertColumns => self.allow.insert_columns,
            Action::DeleteRows => self.allow.delete_rows,
            Action::DeleteColumns => self.allow.delete_columns,
            Action::Sort => self.allow.sort,
            Action::Filter => self.allow.use_autofilter,
            Action::SelectLocked => self.allow.select_locked_cells,
        };
        if allowed {
            None
        } else {
            Some(Denied::SheetAction(action))
        }
    }

    pub fn heap_bytes(&self) -> usize {
        self.unlocked.heap_bytes()
    }
}

// =================================================== workbook protection ==

/// Protection of the workbook's structure — the tab strip, not the cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WorkbookProtection {
    /// Sheets cannot be added, deleted, renamed or reordered.
    structure: bool,
    /// Excel also locks window layout. Carried so a round trip does not drop
    /// it; Ferrix has no windows to lock, so nothing consults it.
    windows: bool,
    hash: PasswordHash,
}

impl WorkbookProtection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn structure_locked(&self) -> bool {
        self.structure
    }

    pub fn windows_locked(&self) -> bool {
        self.windows
    }

    pub fn hash(&self) -> PasswordHash {
        self.hash
    }

    /// Is anything at all protected? Used to decide whether to write the
    /// `<workbookProtection>` element.
    pub fn is_active(&self) -> bool {
        self.structure || self.windows || !self.hash.is_none()
    }

    pub fn protect_structure(&mut self, hash: PasswordHash) {
        self.structure = true;
        self.hash = hash;
    }

    pub fn set_windows(&mut self, on: bool) {
        self.windows = on;
    }

    pub fn unprotect(&mut self) {
        self.structure = false;
        self.windows = false;
        self.hash = PasswordHash::NONE;
    }

    /// Restore state read from a file.
    pub fn from_parts(structure: bool, windows: bool, hash: PasswordHash) -> Self {
        Self {
            structure,
            windows,
            hash,
        }
    }

    /// Why a structural change must be refused, or `None`.
    pub fn deny(&self, op: StructureOp) -> Option<Denied> {
        if self.structure {
            Some(Denied::Structure(op))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
