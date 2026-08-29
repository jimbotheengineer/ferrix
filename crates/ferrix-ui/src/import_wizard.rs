//! The import wizard: encoding, delimiter, quote and header settings, with a
//! live preview, shown when a file will not parse cleanly (issue #31).
//!
//! ## When it appears
//!
//! Never for a file that loads correctly. `ferrix_io::sniff_path` reads a
//! bounded prefix and reports `clean`; only when that is false — a non-UTF-8
//! encoding, a non-comma delimiter, a preamble, a non-standard quote, ragged
//! records — does the wizard open instead of the load starting. The
//! alternative is what the issue is about: a semicolon file silently loading
//! as one 900-character column, which looks like data and is not.
//!
//! ## What the preview costs
//!
//! Nothing that grows with the file. Every re-render calls
//! `ferrix_io::preview_path`, which reads at most `PREFIX_BYTES` and renders
//! at most `PREVIEW_ROWS` records. A 10GB file and a 10KB file cost the same,
//! which is what lets the preview update on every click without debouncing.

use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText};
use ferrix_io::{sniff, CsvOptions};

use crate::theme::Theme;

/// Rows shown in the preview grid.
pub const PREVIEW_ROWS: usize = ferrix_io::PREVIEW_ROWS;

/// A delimiter choice in the wizard.
///
/// `Custom` is a separate variant rather than "any byte" so the radio group
/// can show the four named options without one of them being silently
/// re-selected when the user types a `,` into the custom box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelimChoice {
    Comma,
    Semicolon,
    Tab,
    Pipe,
    Custom,
}

impl DelimChoice {
    pub fn from_byte(b: u8) -> Self {
        match b {
            b',' => Self::Comma,
            b';' => Self::Semicolon,
            b'\t' => Self::Tab,
            b'|' => Self::Pipe,
            _ => Self::Custom,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Comma => "Comma",
            Self::Semicolon => "Semicolon",
            Self::Tab => "Tab",
            Self::Pipe => "Pipe",
            Self::Custom => "Custom",
        }
    }
}

/// Where the header row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderChoice {
    /// First record is the header.
    Yes,
    /// No header at all; every record is data.
    No,
    /// The header is at record N (1-based), and everything before it is
    /// discarded.
    AtRow,
}

/// The wizard's live state.
pub struct ImportWizard {
    /// The file being configured. The wizard never holds its contents.
    pub path: PathBuf,
    /// Why the wizard opened, in the user's words.
    pub reason: String,

    pub encoding_label: String,
    pub delim: DelimChoice,
    /// The byte a `Custom` delimiter means. Kept separately so switching to
    /// Comma and back does not lose what was typed.
    pub custom_delim: String,
    pub quote: String,
    pub header: HeaderChoice,
    /// 1-based header record for [`HeaderChoice::AtRow`].
    pub header_row: usize,
    /// Records discarded before the header.
    pub skip_rows: usize,
    /// Persist these settings against the file's NAME on import.
    pub remember: bool,

    /// The preview last rendered, rebuilt whenever settings change.
    preview: ferrix_io::Preview,
    /// The options `preview` was built from, so a frame that changed nothing
    /// does no work.
    preview_key: Option<PreviewKey>,
    /// Set when the user presses Import; the app consumes it.
    pub accepted: bool,
    /// Set when the user cancels.
    pub cancelled: bool,
}

/// Everything the preview depends on. Cheap to compare, so the preview is
/// rebuilt exactly when it would differ.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PreviewKey {
    delimiter: u8,
    quote: u8,
    has_headers: bool,
    skip_rows: usize,
    encoding: &'static str,
}

impl ImportWizard {
    /// Build a wizard seeded from detection.
    pub fn from_detection(path: &Path, d: &sniff::Detection) -> Self {
        let mut w = Self {
            path: path.to_path_buf(),
            reason: d
                .reason
                .clone()
                .unwrap_or_else(|| "check these settings before importing".to_string()),
            encoding_label: d.encoding.name().to_string(),
            delim: DelimChoice::from_byte(d.delimiter),
            custom_delim: byte_to_text(d.delimiter),
            quote: byte_to_text(d.quote),
            header: if d.has_headers {
                if d.skip_rows > 0 {
                    HeaderChoice::AtRow
                } else {
                    HeaderChoice::Yes
                }
            } else {
                HeaderChoice::No
            },
            header_row: d.skip_rows + 1,
            skip_rows: d.skip_rows,
            remember: false,
            preview: ferrix_io::Preview::default(),
            preview_key: None,
            accepted: false,
            cancelled: false,
        };
        w.refresh_preview();
        w
    }

    /// Build a wizard for a file the user asked to reconfigure, with no
    /// complaint to report.
    pub fn for_path(path: &Path) -> std::io::Result<Self> {
        let d = ferrix_io::sniff_path(path)?;
        let mut w = Self::from_detection(path, &d);
        if d.clean {
            w.reason = "this file already parses cleanly — adjust anything you want to \
                        change"
                .to_string();
        }
        Ok(w)
    }

    /// The delimiter byte the current choices mean.
    pub fn delimiter(&self) -> u8 {
        match self.delim {
            DelimChoice::Comma => b',',
            DelimChoice::Semicolon => b';',
            DelimChoice::Tab => b'\t',
            DelimChoice::Pipe => b'|',
            // An empty or unparseable custom box falls back to a comma rather
            // than to a zero byte, which would make every record one field.
            DelimChoice::Custom => text_to_byte(&self.custom_delim).unwrap_or(b','),
        }
    }

    pub fn quote_byte(&self) -> u8 {
        text_to_byte(&self.quote).unwrap_or(b'"')
    }

    /// Records discarded before the header row.
    ///
    /// `AtRow` and the standalone skip box are the SAME number underneath —
    /// "header at row 4" and "skip 3 then take a header" describe one file.
    /// Deriving both from one field is what stops the two controls
    /// disagreeing about which record the header is.
    pub fn effective_skip(&self) -> usize {
        match self.header {
            HeaderChoice::AtRow => self.header_row.saturating_sub(1),
            _ => self.skip_rows,
        }
    }

    /// The loader options these settings mean.
    pub fn options(&self) -> CsvOptions {
        CsvOptions {
            delimiter: self.delimiter(),
            has_headers: !matches!(self.header, HeaderChoice::No),
            max_rows: None,
            quote: self.quote_byte(),
            skip_rows: self.effective_skip(),
            encoding: ferrix_io::encoding_for_label(&self.encoding_label),
        }
    }

    fn key(&self) -> PreviewKey {
        let o = self.options();
        PreviewKey {
            delimiter: o.delimiter,
            quote: o.quote,
            has_headers: o.has_headers,
            skip_rows: o.skip_rows,
            encoding: o.encoding.map(|e| e.name()).unwrap_or("UTF-8"),
        }
    }

    /// Rebuild the preview if and only if the settings it depends on changed.
    pub fn refresh_preview(&mut self) {
        let key = self.key();
        if self.preview_key == Some(key) {
            return;
        }
        self.preview_key = Some(key);
        // Bounded by construction: `preview_path` reads at most PREFIX_BYTES.
        self.preview =
            ferrix_io::preview_path(&self.path, self.options(), PREVIEW_ROWS).unwrap_or_default();
    }

    pub fn preview(&self) -> &ferrix_io::Preview {
        &self.preview
    }

    /// The key "remember these settings" stores against.
    ///
    /// The file NAME, not the full path, because that is what the issue asks
    /// for and what matches the recurring case: the same weekly export lands
    /// in a different directory (or as `report (1).csv`) every time, and a
    /// path key would make the user re-answer the wizard for a file they have
    /// already configured. Reopening the identical file works either way,
    /// since its name does not change.
    pub fn remember_key(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// A delimiter byte as editable text. Tab is spelled `\t` because a literal
/// tab in a one-character text box is invisible and unenterable.
pub fn byte_to_text(b: u8) -> String {
    match b {
        b'\t' => "\\t".to_string(),
        _ => (b as char).to_string(),
    }
}

/// Inverse of [`byte_to_text`]. `None` for empty or non-ASCII input — a
/// multi-byte character cannot be a delimiter here, and silently taking its
/// first byte would split UTF-8 mid-character.
pub fn text_to_byte(s: &str) -> Option<u8> {
    if s == "\\t" {
        return Some(b'\t');
    }
    if s == "\\0" {
        return None;
    }
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii() {
        return None;
    }
    Some(c as u8)
}

/// Draw the wizard. Returns nothing; the caller inspects `accepted`/
/// `cancelled` and acts.
pub fn show(w: &mut ImportWizard, ctx: &egui::Context, th: &Theme) {
    let mut accept = false;
    let mut cancel = false;

    egui::Window::new("Import settings")
        .collapsible(false)
        .resizable(true)
        .default_width(760.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(
                RichText::new(format!(
                    "{} — {}",
                    w.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    w.reason
                ))
                .color(th.error)
                .size(13.0),
            );
            ui.add_space(6.0);

            egui::Grid::new("import_wizard_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Encoding");
                    egui::ComboBox::from_id_salt("import_encoding")
                        .selected_text(w.encoding_label.clone())
                        .show_ui(ui, |ui| {
                            // The detected encoding is always offered even if
                            // it is not one of the shortlist, so a guess of
                            // e.g. KOI8-R is not silently unselectable.
                            let detected = w.encoding_label.clone();
                            if !ferrix_io::ENCODING_CHOICES.contains(&detected.as_str()) {
                                ui.selectable_value(
                                    &mut w.encoding_label,
                                    detected.clone(),
                                    detected,
                                );
                            }
                            for label in ferrix_io::ENCODING_CHOICES {
                                ui.selectable_value(
                                    &mut w.encoding_label,
                                    label.to_string(),
                                    label,
                                );
                            }
                        });
                    ui.end_row();

                    ui.label("Delimiter");
                    ui.horizontal(|ui| {
                        for c in [
                            DelimChoice::Comma,
                            DelimChoice::Semicolon,
                            DelimChoice::Tab,
                            DelimChoice::Pipe,
                            DelimChoice::Custom,
                        ] {
                            ui.selectable_value(&mut w.delim, c, c.label());
                        }
                        if w.delim == DelimChoice::Custom {
                            ui.add(
                                egui::TextEdit::singleline(&mut w.custom_delim)
                                    .desired_width(28.0)
                                    .char_limit(2),
                            );
                        }
                    });
                    ui.end_row();

                    ui.label("Quote character");
                    ui.add(
                        egui::TextEdit::singleline(&mut w.quote)
                            .desired_width(28.0)
                            .char_limit(2),
                    );
                    ui.end_row();

                    ui.label("Header row");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut w.header, HeaderChoice::Yes, "Yes");
                        ui.selectable_value(&mut w.header, HeaderChoice::No, "No");
                        ui.selectable_value(&mut w.header, HeaderChoice::AtRow, "At row");
                        if w.header == HeaderChoice::AtRow {
                            ui.add(
                                egui::DragValue::new(&mut w.header_row)
                                    .range(1..=1000)
                                    .speed(1.0),
                            );
                        }
                    });
                    ui.end_row();

                    ui.label("Skip leading rows");
                    ui.add_enabled(
                        w.header != HeaderChoice::AtRow,
                        egui::DragValue::new(&mut w.skip_rows)
                            .range(0..=1000)
                            .speed(1.0),
                    )
                    .on_disabled_hover_text(
                        "Set by \"header at row\" — the rows before the header are the \
                         rows skipped.",
                    );
                    ui.end_row();
                });

            ui.add_space(4.0);
            w.refresh_preview();

            let p = w.preview();
            ui.label(
                RichText::new(format!(
                    "Preview · first {} row{} of {} column{}{}",
                    p.rows.len(),
                    if p.rows.len() == 1 { "" } else { "s" },
                    p.cols,
                    if p.cols == 1 { "" } else { "s" },
                    if p.truncated {
                        format!(" · read {} KB of the file", p.prefix_bytes / 1024)
                    } else {
                        String::new()
                    }
                ))
                .color(th.number)
                .size(12.0),
            );
            ui.separator();

            egui::ScrollArea::both()
                .max_height(260.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new("import_preview")
                        .striped(true)
                        .spacing([12.0, 2.0])
                        .show(ui, |ui| {
                            if !p.headers.is_empty() {
                                for h in &p.headers {
                                    ui.label(RichText::new(h).strong());
                                }
                                ui.end_row();
                            }
                            for row in &p.rows {
                                for cell in row {
                                    // Newlines inside a quoted field would
                                    // otherwise make one preview row as tall
                                    // as the field; shown escaped so the row
                                    // grid stays readable.
                                    ui.label(cell.replace('\n', "\\n"));
                                }
                                ui.end_row();
                            }
                        });
                });

            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Import").clicked() {
                    accept = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                let remember_label = format!("Remember these settings for {}", w.remember_key());
                ui.checkbox(&mut w.remember, remember_label).on_hover_text(
                    "Next time a file with this name is opened, it loads straight away \
                     with these settings instead of showing this dialog.",
                );
            });

            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                accept = true;
            }
        });

    if accept {
        w.accepted = true;
    } else if cancel {
        w.cancelled = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimiter_choices_round_trip_through_bytes() {
        for b in *b",;\t|" {
            assert_eq!(text_to_byte(&byte_to_text(b)), Some(b), "byte {b}");
        }
        // A tab must survive the text box, which is why it is spelled \t.
        assert_eq!(byte_to_text(b'\t'), "\\t");
        assert_eq!(text_to_byte("\\t"), Some(b'\t'));
    }

    #[test]
    fn a_multibyte_or_empty_delimiter_is_rejected_not_truncated() {
        // Taking the first byte of "é" would emit a delimiter that splits
        // UTF-8 mid-character, producing replacement chars in every field.
        assert_eq!(text_to_byte("é"), None);
        assert_eq!(text_to_byte(""), None);
        assert_eq!(text_to_byte("ab"), None);
    }

    #[test]
    fn a_custom_delimiter_that_will_not_parse_falls_back_to_comma() {
        let mut w = ImportWizard {
            path: PathBuf::from("x.csv"),
            reason: String::new(),
            encoding_label: "UTF-8".into(),
            delim: DelimChoice::Custom,
            custom_delim: String::new(),
            quote: "\"".into(),
            header: HeaderChoice::Yes,
            header_row: 1,
            skip_rows: 0,
            remember: false,
            preview: ferrix_io::Preview::default(),
            preview_key: None,
            accepted: false,
            cancelled: false,
        };
        assert_eq!(w.delimiter(), b',', "an empty custom box must not be 0x00");
        w.custom_delim = "~".into();
        assert_eq!(w.delimiter(), b'~');
    }

    #[test]
    fn header_at_row_and_skip_rows_are_one_number() {
        let mut w = ImportWizard {
            path: PathBuf::from("x.csv"),
            reason: String::new(),
            encoding_label: "UTF-8".into(),
            delim: DelimChoice::Comma,
            custom_delim: ",".into(),
            quote: "\"".into(),
            header: HeaderChoice::AtRow,
            header_row: 4,
            skip_rows: 0,
            remember: false,
            preview: ferrix_io::Preview::default(),
            preview_key: None,
            accepted: false,
            cancelled: false,
        };
        // "Header at row 4" means three records ahead of it are discarded.
        assert_eq!(w.effective_skip(), 3);
        assert!(w.options().has_headers);

        w.header = HeaderChoice::No;
        w.skip_rows = 2;
        assert_eq!(w.effective_skip(), 2);
        assert!(!w.options().has_headers);
    }

    #[test]
    fn remember_key_is_the_file_name() {
        let w = ImportWizard {
            path: PathBuf::from("/some/deep/dir/quarterly report.csv"),
            reason: String::new(),
            encoding_label: "UTF-8".into(),
            delim: DelimChoice::Comma,
            custom_delim: ",".into(),
            quote: "\"".into(),
            header: HeaderChoice::Yes,
            header_row: 1,
            skip_rows: 0,
            remember: false,
            preview: ferrix_io::Preview::default(),
            preview_key: None,
            accepted: false,
            cancelled: false,
        };
        assert_eq!(w.remember_key(), "quarterly report.csv");
    }
}
