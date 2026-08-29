//! Recent files, start screen templates, and per-file session restore (#45).
//!
//! Three things that all hang off the same fact — *which file is open* — and
//! so share one module rather than being scattered through `app.rs`:
//!
//! * the **recent list**, capped at [`MAX_RECENT`] and persisted through
//!   `prefs.rs`;
//! * the **start screen**, offering those recents, a blank workbook, and a
//!   small set of templates; and
//! * the **session** for each recent entry — cursor, scroll, frozen panes —
//!   restored when that file is reopened.
//!
//! ## A missing file is not a gone file
//!
//! An entry whose path does not currently resolve is shown **greyed out and
//! removable**, never silently dropped. A network share that is not mounted
//! yet, a USB disk that is unplugged, and a file that was genuinely deleted
//! are indistinguishable from `Path::exists`, and only one of the three is a
//! reason to forget the user's file. Pruning on sight means the list quietly
//! empties itself every time someone works from a laptop on a train.
//!
//! ## Scale
//!
//! Everything here is bounded by [`MAX_RECENT`] entries, each holding a path
//! and a fixed-size session record. Nothing scales with row count: a restored
//! scroll offset is two numbers, not a materialised viewport.

use std::path::{Path, PathBuf};

use crate::prefs::Prefs;

/// How many files the list remembers.
pub const MAX_RECENT: usize = 15;

/// Where the user was in a file, so reopening it lands them back there.
///
/// Deliberately plain data with a `Default`: a file that has never been opened
/// under this feature restores to the top-left with no freeze, which is
/// exactly what opening a file did before.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Session {
    /// Selection anchor and cursor, as (row, col).
    pub anchor: (u32, u32),
    pub cursor: (u32, u32),
    /// Body scroll offset: fractional row, and horizontal pixels.
    pub scroll_row: f64,
    pub scroll_col_px: f32,
    /// Frozen/split band size, and which of the two it is.
    pub frozen_rows: usize,
    pub frozen_cols: usize,
    /// `true` for a freeze (pinned at 0), `false` for a split.
    pub frozen: bool,
}

/// One remembered file.
#[derive(Clone, Debug, PartialEq)]
pub struct RecentEntry {
    pub path: PathBuf,
    pub session: Session,
}

impl RecentEntry {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            session: Session::default(),
        }
    }

    /// Whether the file is reachable **right now**.
    ///
    /// Called only when the start screen paints (at most [`MAX_RECENT`] stats
    /// per frame it is visible), never from the grid paint loop.
    pub fn is_available(&self) -> bool {
        self.path.exists()
    }

    /// What to show in the list: the file name, falling back to the whole
    /// path for the pathological case of a path with no final component.
    pub fn label(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// The full path, shown on hover.
    pub fn full_path(&self) -> String {
        self.path.display().to_string()
    }
}

/// Move `path` to the front of the list, creating it if new.
///
/// Re-opening a file must not duplicate it, and must not lose the session
/// already recorded against it — the caller records the *new* session when the
/// file is closed or swapped out, not here.
pub fn touch(list: &mut Vec<RecentEntry>, path: &Path) {
    if let Some(i) = list.iter().position(|e| e.path == path) {
        let existing = list.remove(i);
        list.insert(0, existing);
    } else {
        list.insert(0, RecentEntry::new(path));
    }
    list.truncate(MAX_RECENT);
}

/// Forget one entry. This is the ONLY way an entry leaves the list, and it is
/// always the user's explicit choice.
pub fn remove(list: &mut Vec<RecentEntry>, path: &Path) {
    list.retain(|e| e.path != path);
}

/// Record where the user was in `path`.
pub fn set_session(list: &mut Vec<RecentEntry>, path: &Path, session: Session) {
    if let Some(e) = list.iter_mut().find(|e| e.path == path) {
        e.session = session;
    }
}

/// The session remembered for `path`, or the default for a file never seen.
pub fn session_of(list: &[RecentEntry], path: &Path) -> Session {
    list.iter()
        .find(|e| e.path == path)
        .map(|e| e.session.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Serialisation
// ---------------------------------------------------------------------------

/// Percent-escape the characters that would break the `key = value` line
/// format, or the `|` that separates a composite key's two halves.
///
/// The escape set is deliberately minimal so a prefs file stays readable:
/// a Windows path keeps its backslashes and its drive-letter colon, and only
/// the genuinely ambiguous bytes are encoded. Space is included because the
/// parser trims keys, so a leading or trailing space would otherwise be eaten.
pub(crate) fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '=' => out.push_str("%3D"),
            '|' => out.push_str("%7C"),
            ' ' => out.push_str("%20"),
            '"' => out.push_str("%22"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of [`encode_component`]. A malformed escape is left verbatim
/// rather than dropped — a preference file must degrade, never vanish.
pub(crate) fn decode_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        // Not an escape: copy this whole UTF-8 character.
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Serialise the recent list as prefs lines.
///
/// One line per field rather than one packed line per entry: a single
/// unparseable field then costs that field, not the whole entry.
pub(crate) fn to_text(list: &[RecentEntry]) -> String {
    let mut s = String::new();
    for (i, e) in list.iter().enumerate() {
        let p = encode_component(&e.path.display().to_string());
        s.push_str(&format!("recent.{i}.path = {p}\n"));
        let ss = &e.session;
        s.push_str(&format!(
            "recent.{i}.cursor = {},{},{},{}\n",
            ss.anchor.0, ss.anchor.1, ss.cursor.0, ss.cursor.1
        ));
        s.push_str(&format!(
            "recent.{i}.scroll = {},{}\n",
            ss.scroll_row, ss.scroll_col_px
        ));
        s.push_str(&format!(
            "recent.{i}.panes = {},{},{}\n",
            ss.frozen_rows, ss.frozen_cols, ss.frozen
        ));
    }
    s
}

/// Parse one `recent.<idx>.<field> = <value>` line into `list`.
///
/// Entries are addressed by the index in the key, so the four lines of one
/// entry can arrive in any order and a missing line just leaves that field at
/// its default. An index beyond [`MAX_RECENT`] is ignored rather than growing
/// the list — a hand-edited or corrupt file must not be able to make the app
/// allocate an unbounded number of entries.
pub(crate) fn parse_line(list: &mut Vec<RecentEntry>, key: &str, value: &str) {
    let rest = &key["recent.".len()..];
    let Some((idx, field)) = rest.split_once('.') else {
        return;
    };
    let Ok(idx) = idx.trim().parse::<usize>() else {
        return;
    };
    if idx >= MAX_RECENT {
        return;
    }
    while list.len() <= idx {
        list.push(RecentEntry::new(PathBuf::new()));
    }
    let e = &mut list[idx];
    match field.trim() {
        "path" => e.path = PathBuf::from(decode_component(value.trim())),
        "cursor" => {
            let n: Vec<u32> = value
                .split(',')
                .filter_map(|t| t.trim().parse::<u32>().ok())
                .collect();
            if n.len() == 4 {
                e.session.anchor = (n[0], n[1]);
                e.session.cursor = (n[2], n[3]);
            }
        }
        "scroll" => {
            let t: Vec<&str> = value.split(',').collect();
            if t.len() == 2 {
                if let Ok(r) = t[0].trim().parse::<f64>() {
                    e.session.scroll_row = r;
                }
                if let Ok(c) = t[1].trim().parse::<f32>() {
                    e.session.scroll_col_px = c;
                }
            }
        }
        "panes" => {
            let t: Vec<&str> = value.split(',').collect();
            if t.len() == 3 {
                if let Ok(r) = t[0].trim().parse::<usize>() {
                    e.session.frozen_rows = r;
                }
                if let Ok(c) = t[1].trim().parse::<usize>() {
                    e.session.frozen_cols = c;
                }
                e.session.frozen = t[2].trim() == "true";
            }
        }
        _ => {}
    }
}

/// Drop the placeholder entries a sparse or truncated file can leave behind.
///
/// A file listing only `recent.3.path` creates entries 0..3 to reach index 3;
/// those have empty paths and are not files the user ever opened.
pub(crate) fn drop_placeholders(list: &mut Vec<RecentEntry>) {
    list.retain(|e| !e.path.as_os_str().is_empty());
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

/// A starter workbook offered on the start screen.
pub struct Template {
    pub name: &'static str,
    pub description: &'static str,
    /// Column headers.
    pub headers: &'static [&'static str],
    /// Seed rows, as text exactly as if the user had typed them — formulas
    /// included, so a template can demonstrate one.
    pub rows: &'static [&'static [&'static str]],
}

/// The built-in templates.
///
/// Static data, not files on disk: a template that ships as an asset is a
/// template that can go missing. Three is a deliberate floor, not a roadmap —
/// the point of the criterion is that the start screen offers templates at
/// all.
pub fn templates() -> &'static [Template] {
    &[
        Template {
            name: "Budget",
            description: "Monthly income and outgoings with a running total.",
            headers: &["Category", "Planned", "Actual", "Difference"],
            rows: &[
                &["Rent", "1200", "1200", "=C2-B2"],
                &["Groceries", "400", "0", "=C3-B3"],
                &["Transport", "120", "0", "=C4-B4"],
                &["Total", "=SUM(B2:B4)", "=SUM(C2:C4)", "=SUM(D2:D4)"],
            ],
        },
        Template {
            name: "Task tracker",
            description: "Work items with owner, status and due date.",
            headers: &["Task", "Owner", "Status", "Due"],
            rows: &[
                &["Draft proposal", "", "Not started", ""],
                &["Review numbers", "", "Not started", ""],
            ],
        },
        Template {
            name: "Invoice",
            description: "Line items with quantity, unit price and a total.",
            headers: &["Item", "Qty", "Unit price", "Amount"],
            rows: &[
                &["", "1", "0", "=B2*C2"],
                &["", "1", "0", "=B3*C3"],
                &["Total", "", "", "=SUM(D2:D3)"],
            ],
        },
    ]
}

/// What the user picked on the start screen.
#[derive(Clone, Debug, PartialEq)]
pub enum StartChoice {
    /// Open this file.
    Open(PathBuf),
    /// Start an empty workbook.
    Blank,
    /// Start from `templates()[i]`.
    Template(usize),
    /// Show the file picker.
    Browse,
}

/// Paint the start screen and report what, if anything, the user chose.
///
/// Takes `prefs` mutably because "forget this file" is an action taken from
/// this screen, and it must persist immediately — a removal that survives only
/// until the next crash is not a removal.
pub fn show_start_screen(
    ctx: &egui::Context,
    prefs: &mut Prefs,
    th: &crate::theme::Theme,
) -> Option<StartChoice> {
    let mut choice = None;
    let mut forget: Option<PathBuf> = None;

    egui::CentralPanel::default()
        .frame(egui::Frame::none().fill(th.bg).inner_margin(28.0))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new("FERRIX")
                    .color(th.accent)
                    .strong()
                    .size(28.0),
            );
            ui.add_space(14.0);

            ui.columns(2, |cols| {
                // --- recent files ---
                let ui = &mut cols[0];
                ui.label(egui::RichText::new("Recent files").strong().size(15.0));
                ui.add_space(6.0);
                if prefs.recent.is_empty() {
                    ui.label(
                        egui::RichText::new("Nothing yet — open a file to start the list.")
                            .color(th.text_dim),
                    );
                }
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        for e in &prefs.recent {
                            let available = e.is_available();
                            ui.horizontal(|ui| {
                                // A missing file is GREYED, not hidden: the
                                // user gets to decide whether it is gone.
                                let text = if available {
                                    egui::RichText::new(e.label()).color(th.text)
                                } else {
                                    egui::RichText::new(e.label()).color(th.text_dim).italics()
                                };
                                let hover = if available {
                                    e.full_path()
                                } else {
                                    format!(
                                        "{}\n\nNot reachable right now. It may be on a drive \
                                         or share that is not connected — the entry is kept \
                                         so it works again when the drive comes back.",
                                        e.full_path()
                                    )
                                };
                                let b = ui.add(egui::Button::new(text).frame(false));
                                if b.on_hover_text(hover).clicked() && available {
                                    choice = Some(StartChoice::Open(e.path.clone()));
                                }
                                if ui
                                    .small_button("✖")
                                    .on_hover_text("Forget this file")
                                    .clicked()
                                {
                                    forget = Some(e.path.clone());
                                }
                            });
                        }
                    });

                // --- new workbook / templates ---
                let ui = &mut cols[1];
                ui.label(
                    egui::RichText::new("Start something new")
                        .strong()
                        .size(15.0),
                );
                ui.add_space(6.0);
                if ui
                    .button("📄  Blank workbook")
                    .on_hover_text("An empty sheet.")
                    .clicked()
                {
                    choice = Some(StartChoice::Blank);
                }
                if ui
                    .button("📂  Open a file…")
                    .on_hover_text("Browse for a CSV, TSV or xlsx file.")
                    .clicked()
                {
                    choice = Some(StartChoice::Browse);
                }
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Templates").color(th.text_dim));
                ui.add_space(4.0);
                for (i, t) in templates().iter().enumerate() {
                    if ui
                        .button(format!("🗒  {}", t.name))
                        .on_hover_text(t.description)
                        .clicked()
                    {
                        choice = Some(StartChoice::Template(i));
                    }
                }
            });
        });

    if let Some(p) = forget {
        remove(&mut prefs.recent, &p);
        let _ = prefs.save();
    }
    choice
}

#[cfg(test)]
#[path = "recent/tests.rs"]
mod tests;
