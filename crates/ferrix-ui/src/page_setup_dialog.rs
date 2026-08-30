//! The Page Setup dialog (issue #37).
//!
//! ## Why a dialog at all
//!
//! `ferrix_core::page::PageSetup` — paper, orientation, margins, scaling,
//! repeat rows/columns, gridlines, and the three-part header/footer with its
//! `&P`/`&N`/`&D`/`&F` field codes — shipped complete, and the paginator and
//! the PDF/HTML renderers already honour every field. But nothing in the app
//! could *change* it: the model was reachable only from code, so every export
//! came out Letter/portrait with the default margins. This module is the
//! user-facing half — the "model-complete, unreachable" shape this repo has
//! shipped before, closed here for page setup.
//!
//! ## Edit a copy, commit on OK
//!
//! The dialog edits its OWN `PageSetup` and only writes it back to the app when
//! the real OK button is pressed, so Cancel truly cancels. The manual page
//! breaks (`row_breaks`/`col_breaks`, set by dragging in Page Break Preview)
//! are NOT surfaced here and are carried across untouched on commit — the
//! dialog is for the paper-and-margins settings, not the break geometry.

use egui::RichText;
use ferrix_core::page::{Margins, Orientation, PageSetup, PaperSize, Scaling};

use crate::theme::Theme;

/// The live Page Setup dialog: a working copy of the sheet's `PageSetup` plus
/// the free-text scaling fields the two `Scaling` variants need.
pub struct PageSetupState {
    /// The working copy. Committed to the app on OK, dropped on Cancel.
    pub setup: PageSetup,
    /// `Fit to N pages wide` — empty means "unconstrained on this axis".
    /// Held as text so a half-typed value does not snap to a default.
    pub fit_wide: String,
    /// `Fit to N pages tall`.
    pub fit_tall: String,
    /// `Scale to N %` for the `Percent` variant.
    pub percent: String,
    /// Which scaling mode the radio buttons select.
    pub fit_mode: bool,
    /// Repeat-rows as an inclusive 1-based range like `1:2`, or empty for none.
    pub repeat_rows: String,
    /// Repeat-cols as an inclusive column-letter range like `A:B`, or empty.
    pub repeat_cols: String,
    /// The last painted rect of the OK and Cancel buttons, so a test can click
    /// the REAL button rather than call the commit handler behind it.
    pub ok_rect: Option<egui::Rect>,
    pub cancel_rect: Option<egui::Rect>,
    /// A problem with the form (bad repeat range, say), shown in red and
    /// blocking OK.
    pub problem: Option<String>,
}

impl PageSetupState {
    /// Open the dialog on a copy of the sheet's current setup, pre-filling the
    /// text fields from whatever scaling/repeat values it already holds.
    pub fn from_setup(setup: &PageSetup) -> Self {
        let (fit_mode, fit_wide, fit_tall, percent) = match setup.scaling {
            Scaling::Percent(p) => (false, String::new(), String::new(), p.to_string()),
            Scaling::FitTo { wide, tall } => (
                true,
                wide.map(|w| w.to_string()).unwrap_or_default(),
                tall.map(|t| t.to_string()).unwrap_or_default(),
                "100".to_string(),
            ),
        };
        PageSetupState {
            fit_mode,
            fit_wide,
            fit_tall,
            percent,
            repeat_rows: range_1based(setup.repeat_rows),
            repeat_cols: cols_letters(setup.repeat_cols),
            ok_rect: None,
            cancel_rect: None,
            problem: None,
            setup: setup.clone(),
        }
    }

    /// Fold the free-text scaling/repeat fields back into `setup`, returning an
    /// error message if any is malformed. Called before committing so OK writes
    /// a fully-resolved `PageSetup`.
    pub fn resolve(&mut self) -> Result<(), String> {
        // Scaling.
        if self.fit_mode {
            let wide = parse_opt_dim(&self.fit_wide).map_err(|_| "Pages wide must be a number")?;
            let tall = parse_opt_dim(&self.fit_tall).map_err(|_| "Pages tall must be a number")?;
            self.setup.scaling = Scaling::FitTo { wide, tall };
        } else {
            let p: u16 = self
                .percent
                .trim()
                .parse()
                .map_err(|_| "Scale must be a whole percentage")?;
            if p == 0 {
                return Err("Scale must be greater than 0%".into());
            }
            self.setup.scaling = Scaling::Percent(p);
        }
        // Repeat rows / cols. Empty clears; a range must be first<=last.
        self.setup.repeat_rows = parse_row_range(&self.repeat_rows)?;
        self.setup.repeat_cols = parse_col_range(&self.repeat_cols)?;
        Ok(())
    }
}

/// A 0-based inclusive pair rendered as a 1-based `1:2` for display.
fn range_1based(r: Option<(u32, u32)>) -> String {
    match r {
        Some((a, b)) => format!("{}:{}", a + 1, b + 1),
        None => String::new(),
    }
}

/// A 0-based inclusive column pair rendered as `A:B`.
fn cols_letters(r: Option<(u32, u32)>) -> String {
    match r {
        Some((a, b)) => format!("{}:{}", col_name(a), col_name(b)),
        None => String::new(),
    }
}

/// 0-based column index to spreadsheet letters (0 -> A, 26 -> AA).
fn col_name(mut c: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

/// Parse spreadsheet column letters to a 0-based index (`A` -> 0, `AA` -> 26).
fn parse_col_name(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let mut acc: u32 = 0;
    for b in s.bytes() {
        let d = (b.to_ascii_uppercase() - b'A') as u32 + 1;
        acc = acc.checked_mul(26)?.checked_add(d)?;
    }
    Some(acc - 1)
}

/// Empty string, "5", or "" -> None; a number -> Some, rejecting zero and junk.
fn parse_opt_dim(s: &str) -> Result<Option<u16>, ()> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let n: u16 = s.parse().map_err(|_| ())?;
    if n == 0 {
        Ok(None)
    } else {
        Ok(Some(n))
    }
}

/// Parse a 1-based inclusive row range like `1:2` to a 0-based pair. Empty is
/// None (no repeat). Rejects reversed or non-numeric ranges.
fn parse_row_range(s: &str) -> Result<Option<(u32, u32)>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let (a, b) = split_pair(s).ok_or("Repeat rows must look like 1:2")?;
    let a: u32 = a
        .trim()
        .parse()
        .map_err(|_| "Repeat rows must be numbers")?;
    let b: u32 = b
        .trim()
        .parse()
        .map_err(|_| "Repeat rows must be numbers")?;
    if a == 0 || b == 0 {
        return Err("Rows are numbered from 1".into());
    }
    if a > b {
        return Err("Repeat rows: first row must not exceed the last".into());
    }
    Ok(Some((a - 1, b - 1)))
}

/// Parse a column-letter range like `A:B` to a 0-based pair.
fn parse_col_range(s: &str) -> Result<Option<(u32, u32)>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let (a, b) = split_pair(s).ok_or("Repeat columns must look like A:B")?;
    let a = parse_col_name(a).ok_or("Repeat columns must be letters like A:B")?;
    let b = parse_col_name(b).ok_or("Repeat columns must be letters like A:B")?;
    if a > b {
        return Err("Repeat columns: first column must not exceed the last".into());
    }
    Ok(Some((a, b)))
}

/// Split `"a:b"` into its two halves, accepting a bare `"a"` as `("a","a")`.
fn split_pair(s: &str) -> Option<(&str, &str)> {
    match s.split_once(':') {
        Some((a, b)) => Some((a, b)),
        None => Some((s, s)),
    }
}

/// Paint the dialog. Returns `Some(true)` when OK committed a new setup,
/// `Some(false)` when the dialog was cancelled/closed, `None` while it stays
/// open. The caller owns the working state and writes `state.setup` back to the
/// sheet on `Some(true)`.
pub fn show(ctx: &egui::Context, th: &Theme, state: &mut PageSetupState) -> Option<bool> {
    let mut ok = false;
    let mut cancel = false;

    egui::Window::new("Page Setup")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            egui::Grid::new("page_setup_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Paper");
                    egui::ComboBox::from_id_salt("page_setup_paper")
                        .selected_text(state.setup.paper.label())
                        .show_ui(ui, |ui| {
                            for &p in PaperSize::all() {
                                ui.selectable_value(&mut state.setup.paper, p, p.label());
                            }
                        });
                    ui.end_row();

                    ui.label("Orientation");
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut state.setup.orientation,
                            Orientation::Portrait,
                            "Portrait",
                        );
                        ui.selectable_value(
                            &mut state.setup.orientation,
                            Orientation::Landscape,
                            "Landscape",
                        );
                    });
                    ui.end_row();

                    ui.label("Margins");
                    ui.horizontal(|ui| {
                        if ui.button("Normal").clicked() {
                            state.setup.margins = Margins::default();
                        }
                        if ui.button("Narrow").clicked() {
                            state.setup.margins = Margins::narrow();
                        }
                        if ui.button("Wide").clicked() {
                            state.setup.margins = Margins::wide();
                        }
                    });
                    ui.end_row();

                    ui.label("Scaling");
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut state.fit_mode, false, "Scale");
                            ui.add_enabled(
                                !state.fit_mode,
                                egui::TextEdit::singleline(&mut state.percent).desired_width(48.0),
                            );
                            ui.label("%");
                        });
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut state.fit_mode, true, "Fit to");
                            ui.add_enabled(
                                state.fit_mode,
                                egui::TextEdit::singleline(&mut state.fit_wide).desired_width(36.0),
                            );
                            ui.label("wide ×");
                            ui.add_enabled(
                                state.fit_mode,
                                egui::TextEdit::singleline(&mut state.fit_tall).desired_width(36.0),
                            );
                            ui.label("tall (blank = any)");
                        });
                    });
                    ui.end_row();

                    ui.label("Repeat rows");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.repeat_rows)
                            .hint_text("1:2")
                            .desired_width(90.0),
                    );
                    ui.end_row();

                    ui.label("Repeat columns");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.repeat_cols)
                            .hint_text("A:B")
                            .desired_width(90.0),
                    );
                    ui.end_row();

                    ui.label("Gridlines");
                    ui.checkbox(&mut state.setup.gridlines, "Print gridlines");
                    ui.end_row();

                    ui.label("Headings");
                    ui.checkbox(&mut state.setup.headings, "Print row/column headings");
                    ui.end_row();
                });

            ui.add_space(4.0);
            ui.separator();
            ui.label(RichText::new("Header").strong());
            hf_row(ui, &mut state.setup.header);
            ui.add_space(2.0);
            ui.label(RichText::new("Footer").strong());
            hf_row(ui, &mut state.setup.footer);
            ui.label(
                RichText::new(
                    "Field codes: &P page · &N total · &D date · &T time · &F file · &A sheet",
                )
                .color(th.text_dim)
                .size(11.5),
            );

            if let Some(p) = &state.problem {
                ui.add_space(4.0);
                ui.label(RichText::new(p).color(th.error).size(12.5));
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let r = ui.button("OK");
                state.ok_rect = Some(r.rect);
                if r.clicked() {
                    ok = true;
                }
                let c = ui.button("Cancel");
                state.cancel_rect = Some(c.rect);
                if c.clicked() {
                    cancel = true;
                }
            });

            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
        });

    if ok {
        // Resolve the free-text fields; a bad value keeps the dialog open with
        // the reason shown rather than committing a half-parsed setup.
        match state.resolve() {
            Ok(()) => {
                state.problem = None;
                return Some(true);
            }
            Err(msg) => {
                state.problem = Some(msg);
                return None;
            }
        }
    }
    if cancel {
        return Some(false);
    }
    None
}

/// The three-part left/centre/right editor a header or footer shares.
fn hf_row(ui: &mut egui::Ui, hf: &mut ferrix_core::page::HeaderFooter) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut hf.left)
                .hint_text("left")
                .desired_width(120.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut hf.center)
                .hint_text("centre")
                .desired_width(120.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut hf.right)
                .hint_text("right")
                .desired_width(120.0),
        );
    });
}

#[cfg(test)]
mod tests;
