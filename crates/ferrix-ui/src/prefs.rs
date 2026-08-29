//! Tiny preferences file for settings that must survive a restart.
//!
//! Deliberately hand-rolled rather than pulling in a config crate: this is two
//! booleans and an enum. The format is `key = value`, one per line, and an
//! unreadable or half-written file is simply ignored — a corrupt preference
//! must never stop the app from opening.
//!
//! ## Where it lives
//!
//! The platform's per-user config directory, resolved from environment
//! variables so no extra dependency is needed:
//!
//! | platform | location                                  |
//! |----------|-------------------------------------------|
//! | Windows  | `%APPDATA%\ferrix\prefs.toml`             |
//! | macOS    | `$HOME/Library/Application Support/ferrix` |
//! | Linux    | `$XDG_CONFIG_HOME/ferrix`, else `~/.config/ferrix` |
//!
//! `FERRIX_CONFIG_DIR` overrides all of it, which is also what lets the tests
//! exercise the real round-trip against a temp directory.

use std::path::{Path, PathBuf};

use crate::recent::RecentEntry;
use crate::theme::ThemeMode;

/// Everything persisted between runs.
///
/// `PartialEq` but not `Eq`: `zoom` is an f32. Comparisons here are exact by
/// design — a zoom is only ever one of a fixed set of stops, or a value that
/// round-tripped through this file's own formatting.
#[derive(Clone, Debug, PartialEq)]
pub struct Prefs {
    /// `None` means "never chosen" — the caller then follows the OS.
    pub theme: Option<ThemeMode>,
    /// Issue #20: show empty padding rows past the end of the sheet.
    pub show_empty_rows: bool,
    /// Autosave cadence in seconds. `None` means "not configured", and the
    /// app uses `DEFAULT_AUTOSAVE_SECS`. Zero disables autosave entirely.
    pub autosave_secs: Option<u64>,
    /// Zoom level per sheet, keyed by `(workbook path, sheet name)`.
    ///
    /// Ids are assigned per run and mean nothing across a restart; the name is
    /// what the user recognises and what survives reopening the file. Only
    /// sheets the user actually zoomed appear, so the default costs nothing.
    ///
    /// The workbook path is part of the key (issue #45). Keyed on the sheet
    /// name ALONE, every workbook with a sheet called `Sheet1` — which is most
    /// of them — shared one zoom, so zooming into one file silently re-zoomed
    /// every other file the user opened. The entries are
    /// `(workbook path, sheet name, zoom)`; an in-memory workbook with no file
    /// behind it yet uses an empty path, which is a key like any other.
    pub zoom: Vec<(String, String, f32)>,
    /// Recently opened files, newest first, with the session to restore for
    /// each. Capped at `recent::MAX_RECENT` (issue #45).
    pub recent: Vec<RecentEntry>,
    /// Issue #40: command palette recency, most-recently-run first, stored as
    /// command SLUGS rather than indices — a registry reorder must not
    /// silently reassign somebody's history to different commands.
    pub recent_commands: Vec<String>,
    /// Issue #38: how many text rows tall the formula bar is. 1 is the
    /// classic single-line bar.
    ///
    /// This is why `Prefs` no longer derives `Default`: a derived default
    /// would be ZERO rows, i.e. a formula bar with no formula bar in it. The
    /// default has to be 1, and a hand-written impl is the only way to say so
    /// once rather than at every construction site.
    pub formula_bar_rows: usize,
    /// Issue #31: import settings the user chose to remember, keyed by FILE
    /// NAME. When a file whose name is here is opened, the import wizard is
    /// skipped and these settings are used.
    ///
    /// Keyed by name rather than full path deliberately — the recurring case
    /// is the same weekly export arriving in a new directory, and a path key
    /// makes the user re-answer the wizard for a file they configured last
    /// week.
    pub import_rules: Vec<ImportRule>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: None,
            show_empty_rows: false,
            autosave_secs: None,
            zoom: Vec::new(),
            recent: Vec::new(),
            recent_commands: Vec::new(),
            formula_bar_rows: 1,
            import_rules: Vec::new(),
        }
    }
}

impl Prefs {
    /// The cadence actually used, resolving the unset case to the default.
    pub fn autosave_interval(&self) -> std::time::Duration {
        let secs = self
            .autosave_secs
            .unwrap_or(ferrix_io::edits::DEFAULT_AUTOSAVE_SECS);
        std::time::Duration::from_secs(secs)
    }

    /// Autosave is off when the user explicitly set the interval to zero.
    pub fn autosave_enabled(&self) -> bool {
        self.autosave_secs != Some(0)
    }
}

impl Prefs {
    /// Zoom remembered for a sheet of a particular workbook, or 100% when it
    /// was never set.
    pub fn zoom_of(&self, book: &Path, sheet: &str) -> f32 {
        let book = book.display().to_string();
        self.zoom
            .iter()
            .find(|(b, n, _)| b == &book && n == sheet)
            .map(|&(_, _, z)| crate::grid::clamp_zoom(z))
            .unwrap_or(1.0)
    }

    /// Remember a sheet's zoom within a workbook. 100% is the default, so it
    /// is REMOVED rather than stored — otherwise the file would accrue an
    /// entry for every sheet the user ever glanced at.
    pub fn set_zoom(&mut self, book: &Path, sheet: &str, zoom: f32) {
        let book = book.display().to_string();
        let z = crate::grid::clamp_zoom(zoom);
        self.zoom.retain(|(b, n, _)| !(b == &book && n == sheet));
        if (z - 1.0).abs() > 1e-4 {
            self.zoom.push((book, sheet.to_string(), z));
        }
    }
}

/// Remembered import settings for one file name (issue #31).
///
/// Stored as plain fields rather than a `CsvOptions`: the preferences file
/// must round-trip through text, and `&'static Encoding` is not something a
/// parse can conjure — the encoding travels as its label and is resolved on
/// use. An unknown label resolves to `None`, i.e. UTF-8, which is the same
/// thing an older build would have done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRule {
    /// The file NAME this applies to, e.g. `weekly-export.csv`.
    pub name: String,
    pub encoding: String,
    pub delimiter: u8,
    pub quote: u8,
    pub has_headers: bool,
    pub skip_rows: usize,
}

impl ImportRule {
    /// The loader options this rule means.
    pub fn to_options(&self) -> ferrix_io::CsvOptions {
        ferrix_io::CsvOptions {
            delimiter: self.delimiter,
            has_headers: self.has_headers,
            max_rows: None,
            quote: self.quote,
            skip_rows: self.skip_rows,
            encoding: ferrix_io::encoding_for_label(&self.encoding),
        }
    }
}

impl Prefs {
    /// The remembered rule for a path, matched on its file NAME.
    pub fn import_rule_for(&self, path: &Path) -> Option<&ImportRule> {
        let name = path.file_name()?.to_string_lossy().into_owned();
        self.import_rules.iter().find(|r| r.name == name)
    }

    /// Remember `rule`, replacing any previous rule for the same name.
    ///
    /// Replace rather than append: two rules for one name would make which
    /// settings a file loads with depend on iteration order, and the user
    /// would see their newest choice silently ignored.
    pub fn set_import_rule(&mut self, rule: ImportRule) {
        self.import_rules.retain(|r| r.name != rule.name);
        self.import_rules.push(rule);
    }

    /// Forget the rule for a file name, so the wizard shows again.
    ///
    /// Returns whether anything was removed, so a caller can avoid writing
    /// the preferences file when there was nothing to forget. That matters:
    /// an unconditional write here would turn every file open into a prefs
    /// WRITE, which is exactly the shared-state hazard issue #45 hit.
    pub fn clear_import_rule(&mut self, name: &str) -> bool {
        let before = self.import_rules.len();
        self.import_rules.retain(|r| r.name != name);
        before != self.import_rules.len()
    }
}

const FILE: &str = "prefs.toml";

pub fn config_dir() -> Option<PathBuf> {
    // A PER-THREAD override, checked before the process-wide env var.
    //
    // Integration defect this exists to fix (#40 + #45): `FERRIX_CONFIG_DIR`
    // is process-wide, so it can only isolate tests if EVERY prefs-writing
    // test serialises on `CONFIG_ENV_LOCK`. Issue #45 made a completed file
    // load call `persist_prefs()`, which silently turned every harness test
    // that opens a file into a prefs WRITER — including the many that never
    // took the lock. Those writes landed in whatever directory the env var
    // happened to point at, so an unrelated test could truncate the prefs
    // file a lock-holding test was mid-way through round-tripping. It
    // presented as `recent_commands` coming back empty after a "restart".
    //
    // Each test runs on its own thread, so a thread-local override isolates
    // by construction rather than by every author remembering a lock.
    #[cfg(test)]
    if let Some(dir) = test_config_dir() {
        return Some(dir);
    }
    if let Some(dir) = std::env::var_os("FERRIX_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library").join("Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.map(|b| b.join("ferrix"))
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// This thread's config directory, if it has claimed one.
#[cfg(test)]
pub(crate) fn test_config_dir() -> Option<PathBuf> {
    TEST_CONFIG_DIR.with(|d| d.borrow().clone())
}

/// Point THIS THREAD's prefs at `dir`, leaving every other thread alone.
///
/// Returns the previous value so a guard can restore it. Unlike setting
/// `FERRIX_CONFIG_DIR`, this needs no process-wide lock.
#[cfg(test)]
pub(crate) fn set_test_config_dir(dir: Option<PathBuf>) -> Option<PathBuf> {
    TEST_CONFIG_DIR.with(|d| std::mem::replace(&mut *d.borrow_mut(), dir))
}

/// Formula bar height bounds, in text rows (issue #38).
///
/// The floor is 1 because a zero-row bar cannot be dragged back open. The
/// ceiling exists because the bar is a `TopBottomPanel`: an unbounded drag
/// would push the grid off the bottom of the window entirely.
pub const MIN_FORMULA_BAR_ROWS: usize = 1;
pub const MAX_FORMULA_BAR_ROWS: usize = 12;

fn prefs_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(FILE))
}

impl Prefs {
    /// Read preferences, falling back to defaults on any failure. A missing,
    /// unreadable, or malformed file is not an error the user should see.
    pub fn load() -> Self {
        let Some(p) = prefs_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(p) else {
            return Self::default();
        };
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"');
            match k.trim() {
                // An unrecognised value leaves `theme` as None, so the app
                // falls back to the OS preference rather than to a guess.
                "theme" => out.theme = ThemeMode::parse(v),
                "show_empty_rows" => out.show_empty_rows = v == "true",
                // Clamped on the way IN, not just on the way out: a hand-edited
                // `formula_bar_rows = 0` must not produce an invisible formula
                // bar the user then cannot use to fix anything.
                "formula_bar_rows" => {
                    if let Ok(n) = v.parse::<usize>() {
                        out.formula_bar_rows = n.clamp(MIN_FORMULA_BAR_ROWS, MAX_FORMULA_BAR_ROWS);
                    }
                }
                // A malformed number leaves this None, i.e. the default
                // cadence — never zero, which would silently disable
                // autosave because of a typo in a config file.
                "autosave_secs" => out.autosave_secs = v.parse::<u64>().ok(),
                // Issue #40: `recent_commands = a,b,c`, most recent first.
                // Unknown slugs are kept here and filtered where they are
                // resolved, so a preference written by a newer build is not
                // destroyed by an older one reading it.
                "recent_commands" => {
                    out.recent_commands = v
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                // `zoom.<path>|<sheet name> = 2` — one line per zoomed sheet
                // of one workbook. Both halves are percent-escaped by
                // `recent::encode_component`, so the '|' that splits them, and
                // any '=' or space inside either half, cannot be confused with
                // the format's own punctuation. A Windows drive letter's colon
                // and its backslashes pass through unescaped and readable.
                k2 if k2.starts_with("zoom.") => {
                    let key = k2.trim_start_matches("zoom.").trim().trim_matches('"');
                    // BACKWARD COMPATIBILITY: an old-format `zoom.<sheet>`
                    // line has no '|' and therefore names no workbook. There
                    // is nothing to migrate it to — attaching it to a guessed
                    // workbook would re-create the exact cross-file bleed this
                    // keying exists to stop — so it is DROPPED. The user loses
                    // one remembered zoom, re-zooms once, and is right after.
                    let Some((book, name)) = key.split_once('|') else {
                        continue;
                    };
                    let book = crate::recent::decode_component(book);
                    let name = crate::recent::decode_component(name);
                    if let Ok(z) = v.parse::<f32>() {
                        if !name.is_empty() {
                            out.set_zoom(Path::new(&book), &name, z);
                        }
                    }
                }
                // `recent.<n>.<field> = ...` — the recent-files list and the
                // session remembered for each entry (issue #45).
                // `import.<name> = enc|delim|quote|headers|skip` — remembered
                // wizard settings for one file name (issue #31). The name is
                // percent-escaped by `recent::encode_component`, so a name
                // containing '|', '=' or a newline cannot forge another key.
                k2 if k2.starts_with("import.") => {
                    let name = k2.trim_start_matches("import.").trim().trim_matches('"');
                    let name = crate::recent::decode_component(name);
                    if let Some(rule) = parse_import_rule(&name, v) {
                        out.set_import_rule(rule);
                    }
                }
                k2 if k2.starts_with("recent.") => {
                    crate::recent::parse_line(&mut out.recent, k2.trim(), v);
                }
                _ => {}
            }
        }
        crate::recent::drop_placeholders(&mut out.recent);
        out
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from("# Ferrix preferences\n");
        if let Some(t) = self.theme {
            s.push_str(&format!("theme = \"{}\"\n", t.as_str()));
        }
        s.push_str(&format!("show_empty_rows = {}\n", self.show_empty_rows));
        s.push_str(&format!("formula_bar_rows = {}\n", self.formula_bar_rows));
        if let Some(secs) = self.autosave_secs {
            s.push_str(&format!("autosave_secs = {secs}\n"));
        }
        // Issue #40. Omitted entirely when nothing has been run, so a fresh
        // install's file gains no line. Slugs contain no ',' or newline by
        // construction, but both are stripped anyway — a preference file must
        // never be able to forge an entry.
        if !self.recent_commands.is_empty() {
            let joined = self
                .recent_commands
                .iter()
                .map(|c| c.replace([',', '\n', '\r'], ""))
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
                .join(",");
            s.push_str(&format!("recent_commands = {joined}\n"));
        }
        for (book, name, z) in &self.zoom {
            // Escaping covers the newline case too: a name carrying a newline
            // would otherwise forge a second key, and a preference file must
            // never be able to inject a setting the user did not choose.
            let book = crate::recent::encode_component(book);
            let name = crate::recent::encode_component(name);
            s.push_str(&format!("zoom.{book}|{name} = {z}\n"));
        }
        // Issue #31. Delimiter and quote go out as DECIMAL BYTE VALUES, not
        // as characters: a tab or a '|' written literally would be
        // indistinguishable from the field separator this format uses.
        for r in &self.import_rules {
            let name = crate::recent::encode_component(&r.name);
            let enc = crate::recent::encode_component(&r.encoding);
            s.push_str(&format!(
                "import.{name} = {enc}|{}|{}|{}|{}\n",
                r.delimiter, r.quote, r.has_headers, r.skip_rows
            ));
        }
        s.push_str(&crate::recent::to_text(&self.recent));
        s
    }

    /// Best-effort write. A failure to persist a preference is not worth
    /// interrupting the user over, so the error is returned for logging and
    /// otherwise dropped by the caller.
    ///
    /// The write is ATOMIC: the bytes go to a sibling temp file which is
    /// flushed and fsync'd, then renamed into place. This follows the
    /// discipline `ferrix_io::edits::write_atomic` already established for the
    /// edit sidecar, and for the same reason — a crash partway through must
    /// leave the PREVIOUS complete file, never a prefix of the new one. A
    /// half-written prefs file parses into a mixture of old and new settings
    /// that looks plausible and is wrong.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(dir) = config_dir() else {
            return Ok(());
        };
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(FILE);
        // A sibling of the destination, so the rename stays within one
        // filesystem and is therefore atomic. The pid keeps two Ferrix
        // processes from writing the same temp file at once.
        let tmp = dir.join(format!("{FILE}.{}.tmp", std::process::id()));
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(self.to_text().as_bytes())?;
            f.flush()?;
            // fsync before the rename: without it the rename can land in the
            // directory while the bytes are still only in the page cache, and
            // a power loss leaves a correctly-named empty file — the very
            // truncation this dance exists to prevent.
            f.sync_all()?;
        }
        // Windows will not rename onto an existing file.
        let _ = std::fs::remove_file(&path);
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Do not leave scratch behind in the user's config directory.
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

/// Parse one `import.<name>` value. `None` for anything malformed — a
/// corrupt rule must degrade to "show the wizard", never to a silently wrong
/// delimiter.
fn parse_import_rule(name: &str, v: &str) -> Option<ImportRule> {
    if name.is_empty() {
        return None;
    }
    let mut parts = v.split('|');
    let encoding = crate::recent::decode_component(parts.next()?);
    let delimiter: u8 = parts.next()?.trim().parse().ok()?;
    let quote: u8 = parts.next()?.trim().parse().ok()?;
    let has_headers = match parts.next()?.trim() {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    let skip_rows: usize = parts.next()?.trim().parse().ok()?;
    // A zero delimiter would make every record one field; a rule that says so
    // is corrupt rather than a preference to honour.
    if delimiter == 0 || quote == 0 {
        return None;
    }
    Some(ImportRule {
        name: name.to_string(),
        encoding,
        delimiter,
        quote,
        has_headers,
        skip_rows,
    })
}

/// Serializes tests that redirect `FERRIX_CONFIG_DIR`.
///
/// The environment is per-PROCESS, and every test in this binary runs in the
/// same one on parallel threads. Two tests each doing set-use-restore will
/// interleave, and the second `set_var` lands between the first test's set and
/// its read — which is exactly how a working persistence feature reports
/// itself broken. Any test that touches this variable must hold this lock.
#[cfg(test)]
pub(crate) static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #38. A missing line must leave the bar at its usable default,
    /// and a hand-edited nonsense value must not be able to produce a formula
    /// bar with no rows in it — which the user could then never drag open.
    #[test]
    fn formula_bar_height_defaults_to_one_row_and_is_clamped_on_read() {
        assert_eq!(Prefs::parse("theme = \"dark\"").formula_bar_rows, 1);
        assert_eq!(Prefs::parse("formula_bar_rows = 0").formula_bar_rows, 1);
        assert_eq!(
            Prefs::parse("formula_bar_rows = 9999").formula_bar_rows,
            MAX_FORMULA_BAR_ROWS
        );
        assert_eq!(Prefs::parse("formula_bar_rows = 5").formula_bar_rows, 5);
        // Garbage leaves the default rather than zeroing the bar.
        assert_eq!(Prefs::parse("formula_bar_rows = tall").formula_bar_rows, 1);
    }

    #[test]
    fn round_trips_through_text() {
        for theme in [None, Some(ThemeMode::Dark), Some(ThemeMode::Light)] {
            for show_empty_rows in [false, true] {
                for autosave_secs in [None, Some(0), Some(15), Some(300)] {
                    let p = Prefs {
                        theme,
                        show_empty_rows,
                        autosave_secs,
                        zoom: Vec::new(),
                        recent: Vec::new(),
                        recent_commands: Vec::new(),
                        formula_bar_rows: 1,
                        import_rules: Vec::new(),
                    };
                    assert_eq!(Prefs::parse(&p.to_text()), p);
                }
                // Zoom entries alongside every other field: two features wrote
                // this test independently, each covering only its own field.
                // A round trip that omits a field cannot catch that field
                // being dropped by the writer.
                let p = Prefs {
                    theme,
                    show_empty_rows,
                    autosave_secs: Some(45),
                    // Keyed on (workbook path, sheet name) since #45. Two
                    // different books both with a "Sheet1" is the case the
                    // old sheet-name-only key collapsed into one.
                    zoom: vec![
                        ("C:\\books\\a.xlsx".into(), "Sheet1".into(), 2.0),
                        ("/mnt/b.csv".into(), "quarterly report".into(), 0.5),
                        ("/mnt/c.csv".into(), "Sheet1".into(), 3.0),
                    ],
                    recent: vec![crate::recent::RecentEntry::new("/mnt/b.csv")],
                    // Recency travels in the same file; a round trip that
                    // omits it cannot catch the writer dropping it.
                    recent_commands: vec!["view.zoom_in".into(), "file.save".into()],
                    // Issue #38 travels in the same file; a round trip that
                    // omits it cannot catch the writer dropping it.
                    formula_bar_rows: 4,
                    // Issue #31 travels in the same file, for the same
                    // reason. A tab delimiter and a '|' quote are here on
                    // purpose: both are the format's own punctuation, so a
                    // writer that emitted them literally would round-trip
                    // into a different rule.
                    import_rules: vec![
                        ImportRule {
                            name: "weekly export.csv".into(),
                            encoding: "windows-1252".into(),
                            delimiter: b'\t',
                            quote: b'|',
                            has_headers: false,
                            skip_rows: 3,
                        },
                        ImportRule {
                            name: "a=b|c.csv".into(),
                            encoding: "UTF-8".into(),
                            delimiter: b';',
                            quote: b'\'',
                            has_headers: true,
                            skip_rows: 0,
                        },
                    ],
                };
                assert_eq!(Prefs::parse(&p.to_text()), p);
            }
        }
    }

    /// Issue #31. A rule is matched on the file NAME, so the same export in a
    /// different directory still skips the wizard.
    #[test]
    fn an_import_rule_matches_by_name_across_directories() {
        let mut p = Prefs::default();
        p.set_import_rule(ImportRule {
            name: "weekly.csv".into(),
            encoding: "windows-1252".into(),
            delimiter: b';',
            quote: b'"',
            has_headers: true,
            skip_rows: 2,
        });
        for dir in ["/downloads", "/archive/2026", "C:\\\\Users\\\\x\\\\Desktop"] {
            let path = Path::new(dir).join("weekly.csv");
            let r = p
                .import_rule_for(&path)
                .unwrap_or_else(|| panic!("no rule for {}", path.display()));
            assert_eq!(r.delimiter, b';');
            assert_eq!(r.skip_rows, 2);
            assert_eq!(r.to_options().skip_rows, 2);
            assert_eq!(
                r.to_options().encoding.map(|e| e.name()),
                Some("windows-1252")
            );
        }
        // A different file must NOT pick up someone else's settings.
        assert!(p
            .import_rule_for(Path::new("/downloads/other.csv"))
            .is_none());
    }

    #[test]
    fn remembering_the_same_name_twice_replaces_rather_than_stacks() {
        let mut p = Prefs::default();
        for delim in *b";\t|" {
            p.set_import_rule(ImportRule {
                name: "x.csv".into(),
                encoding: "UTF-8".into(),
                delimiter: delim,
                quote: b'"',
                has_headers: true,
                skip_rows: 0,
            });
        }
        assert_eq!(p.import_rules.len(), 1, "duplicate rules accumulated");
        assert_eq!(
            p.import_rule_for(Path::new("x.csv")).unwrap().delimiter,
            b'|'
        );
        assert!(
            p.clear_import_rule("x.csv"),
            "clearing must report a change"
        );
        assert!(p.import_rule_for(Path::new("x.csv")).is_none());
        assert!(
            !p.clear_import_rule("x.csv"),
            "clearing nothing must report no change, so no prefs write happens"
        );
    }

    /// A corrupt rule must send the user back to the wizard, never load the
    /// file with a silently wrong delimiter.
    #[test]
    fn a_malformed_import_rule_is_dropped_not_guessed() {
        for bad in [
            "import.x.csv = UTF-8\n",
            "import.x.csv = UTF-8|59\n",
            "import.x.csv = UTF-8|59|34|maybe|0\n",
            "import.x.csv = UTF-8|notanumber|34|true|0\n",
            "import.x.csv = UTF-8|0|34|true|0\n",
            "import. = UTF-8|59|34|true|0\n",
        ] {
            let p = Prefs::parse(bad);
            assert!(
                p.import_rules.is_empty(),
                "{bad:?} parsed into {:?}",
                p.import_rules
            );
        }
    }

    #[test]
    fn garbage_never_panics_and_never_guesses() {
        // An unreadable preference must degrade to "not chosen", which sends
        // the app to the OS preference — not to a silently wrong theme.
        for bad in [
            "",
            "theme",
            "theme = ",
            "theme = mauve\n",
            "!!!\n\0\n= =\n",
            "# only a comment\n",
        ] {
            let p = Prefs::parse(bad);
            assert_eq!(p.theme, None, "parsed {bad:?} into a theme");
            assert!(!p.show_empty_rows);
        }
    }

    #[test]
    fn autosave_defaults_to_thirty_seconds_and_stays_on() {
        let p = Prefs::parse("");
        assert_eq!(p.autosave_secs, None);
        assert_eq!(p.autosave_interval().as_secs(), 30);
        assert!(p.autosave_enabled());
    }

    #[test]
    fn autosave_interval_is_configurable() {
        let p = Prefs::parse("autosave_secs = 5\n");
        assert_eq!(p.autosave_interval().as_secs(), 5);
        assert!(p.autosave_enabled());
    }

    #[test]
    fn zero_disables_autosave_but_a_typo_does_not() {
        // Explicit zero is a deliberate opt-out.
        assert!(!Prefs::parse("autosave_secs = 0\n").autosave_enabled());
        // A malformed value must fall back to the protective default rather
        // than silently leaving the user without autosave.
        for bad in [
            "autosave_secs = later\n",
            "autosave_secs = \n",
            "autosave_secs = -5\n",
        ] {
            let p = Prefs::parse(bad);
            assert!(p.autosave_enabled(), "{bad:?} disabled autosave");
            assert_eq!(p.autosave_interval().as_secs(), 30);
        }
    }

    #[test]
    fn a_truncated_file_still_yields_what_it_can() {
        let full = Prefs {
            theme: Some(ThemeMode::Light),
            show_empty_rows: true,
            autosave_secs: None,
            zoom: vec![("/a.csv".into(), "Sheet1".into(), 3.0)],
            recent: Vec::new(),
            recent_commands: Vec::new(),
            formula_bar_rows: 1,
            import_rules: Vec::new(),
        }
        .to_text();
        let cut = &full[..full.len() - 8];
        let p = Prefs::parse(cut);
        assert_eq!(p.theme, Some(ThemeMode::Light), "lost a complete line");
    }

    /// The actual acceptance criterion: the preference survives a restart.
    /// This does the real filesystem round-trip through `save`/`load`, with
    /// the config dir redirected at a temp directory.
    #[test]
    fn theme_preference_survives_a_restart() {
        // Held for the whole test: the env var is process-wide and shared
        // with every other test in this binary.
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ferrix-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Set-and-restore rather than leaving it set: other tests in this
        // binary share the process environment.
        let prev = std::env::var_os("FERRIX_CONFIG_DIR");
        std::env::set_var("FERRIX_CONFIG_DIR", &dir);

        let want = Prefs {
            theme: Some(ThemeMode::Light),
            show_empty_rows: true,
            autosave_secs: Some(45),
            zoom: vec![("/a.csv".into(), "Sheet1".into(), 2.0)],
            recent: vec![crate::recent::RecentEntry::new("/a.csv")],
            recent_commands: vec!["data.goal_seek".into(), "view.theme".into()],
            // Issue #38: the formula bar's height is part of the same
            // restart-survival criterion as the theme.
            formula_bar_rows: 3,
            // Issue #31: a remembered import rule must survive a restart too,
            // or "remember these settings" only lasts until the app closes.
            import_rules: vec![ImportRule {
                name: "a.csv".into(),
                encoding: "windows-1252".into(),
                delimiter: b';',
                quote: b'"',
                has_headers: true,
                skip_rows: 2,
            }],
        };
        want.save().expect("save");
        // A fresh `load` is exactly what the next process run does.
        assert_eq!(Prefs::load(), want);

        match prev {
            Some(v) => std::env::set_var("FERRIX_CONFIG_DIR", v),
            None => std::env::remove_var("FERRIX_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
