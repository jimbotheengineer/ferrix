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

use std::path::PathBuf;

use crate::theme::ThemeMode;

/// Everything persisted between runs.
///
/// `PartialEq` but not `Eq`: `zoom` is an f32. Comparisons here are exact by
/// design — a zoom is only ever one of a fixed set of stops, or a value that
/// round-tripped through this file's own formatting.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Prefs {
    /// `None` means "never chosen" — the caller then follows the OS.
    pub theme: Option<ThemeMode>,
    /// Issue #20: show empty padding rows past the end of the sheet.
    pub show_empty_rows: bool,
    /// Autosave cadence in seconds. `None` means "not configured", and the
    /// app uses `DEFAULT_AUTOSAVE_SECS`. Zero disables autosave entirely.
    pub autosave_secs: Option<u64>,
    /// Zoom level per sheet, keyed by sheet NAME rather than by `SheetId`.
    ///
    /// Ids are assigned per run and mean nothing across a restart; the name is
    /// what the user recognises and what survives reopening the file. Only
    /// sheets the user actually zoomed appear, so the default costs nothing.
    pub zoom: Vec<(String, f32)>,
    /// Issue #40: command palette recency, most-recently-run first, stored as
    /// command SLUGS rather than indices — a registry reorder must not
    /// silently reassign somebody's history to different commands.
    pub recent_commands: Vec<String>,
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
    /// Zoom remembered for a sheet, or 100% when it was never set.
    pub fn zoom_of(&self, sheet: &str) -> f32 {
        self.zoom
            .iter()
            .find(|(n, _)| n == sheet)
            .map(|&(_, z)| crate::grid::clamp_zoom(z))
            .unwrap_or(1.0)
    }

    /// Remember a sheet's zoom. 100% is the default, so it is REMOVED rather
    /// than stored — otherwise the file would accrue an entry for every sheet
    /// the user ever glanced at.
    pub fn set_zoom(&mut self, sheet: &str, zoom: f32) {
        let z = crate::grid::clamp_zoom(zoom);
        self.zoom.retain(|(n, _)| n != sheet);
        if (z - 1.0).abs() > 1e-4 {
            self.zoom.push((sheet.to_string(), z));
        }
    }
}

const FILE: &str = "prefs.toml";

pub fn config_dir() -> Option<PathBuf> {
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
                // `zoom.<sheet name> = 2` — one line per zoomed sheet. A name
                // containing '=' still parses: `split_once` takes the FIRST
                // '=', and the key is everything before it.
                k2 if k2.starts_with("zoom.") => {
                    let name = k2.trim_start_matches("zoom.").trim().trim_matches('"');
                    if let Ok(z) = v.parse::<f32>() {
                        if !name.is_empty() {
                            out.set_zoom(name, z);
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from("# Ferrix preferences\n");
        if let Some(t) = self.theme {
            s.push_str(&format!("theme = \"{}\"\n", t.as_str()));
        }
        s.push_str(&format!("show_empty_rows = {}\n", self.show_empty_rows));
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
        for (name, z) in &self.zoom {
            // Newlines in a sheet name would forge a second key, so they are
            // dropped rather than written — a preference file must never be
            // able to inject a setting the user did not choose.
            let name = name.replace(['\n', '\r'], " ");
            s.push_str(&format!("zoom.{name} = {z}\n"));
        }
        s
    }

    /// Best-effort write. A failure to persist a preference is not worth
    /// interrupting the user over, so the error is returned for logging and
    /// otherwise dropped by the caller.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(dir) = config_dir() else {
            return Ok(());
        };
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(FILE), self.to_text())
    }
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
                        recent_commands: Vec::new(),
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
                    zoom: vec![("Sheet1".into(), 2.0), ("quarterly report".into(), 0.5)],
                    // Recency travels in the same file; a round trip that
                    // omits it cannot catch the writer dropping it.
                    recent_commands: vec!["view.zoom_in".into(), "file.save".into()],
                };
                assert_eq!(Prefs::parse(&p.to_text()), p);
            }
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
            zoom: vec![("Sheet1".into(), 3.0)],
            recent_commands: Vec::new(),
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
            zoom: vec![("Sheet1".into(), 2.0)],
            recent_commands: vec!["data.goal_seek".into(), "view.theme".into()],
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
