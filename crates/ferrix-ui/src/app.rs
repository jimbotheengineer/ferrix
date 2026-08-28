//! Application state and top-level layout.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use egui::{Align, Layout, RichText};
use ferrix_core::{CellRef, Sheet, Value};
use ferrix_formula::{eval, parse};
use ferrix_io::{load_csv, CsvOptions, LoadStats};

use crate::grid::{Grid, DEFAULT_COL_WIDTH};
use crate::theme::Theme;

/// Result of a background load.
type LoadResult = Result<(Sheet, LoadStats), String>;

pub struct FerrixApp {
    sheet: Sheet,
    stats: Option<LoadStats>,
    col_widths: Vec<f32>,
    selection: Option<CellRef>,
    /// Contents of the formula bar (edited independently of the cell).
    formula_input: String,
    /// Result of evaluating the formula bar, shown live.
    formula_result: Option<String>,
    status: String,
    loading: bool,
    load_rx: Option<Receiver<LoadResult>>,
    /// Rolling frame time for the FPS readout.
    frame_ms: f32,
    last_painted: usize,
}

impl FerrixApp {
    pub fn new(initial: Option<PathBuf>) -> Self {
        let mut app = Self {
            sheet: Sheet::new("Sheet1"),
            stats: None,
            col_widths: Vec::new(),
            selection: Some(CellRef::new(0, 0)),
            formula_input: String::new(),
            formula_result: None,
            status: "Ready — open a CSV to begin".into(),
            loading: false,
            load_rx: None,
            frame_ms: 0.0,
            last_painted: 0,
        };
        if let Some(p) = initial {
            app.start_load(p);
        }
        app
    }

    /// Kick off a load on a worker thread so the UI never blocks — a 10M-row
    /// file takes seconds to parse and the window must stay responsive.
    fn start_load(&mut self, path: PathBuf) {
        let (tx, rx) = channel();
        self.loading = true;
        self.status = format!("Loading {}…", path.display());
        std::thread::spawn(move || {
            let result = load_csv(&path, CsvOptions::default()).map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.load_rx = Some(rx);
    }

    fn poll_load(&mut self) {
        let Some(rx) = &self.load_rx else { return };
        match rx.try_recv() {
            Ok(Ok((sheet, stats))) => {
                self.col_widths = compute_col_widths(&sheet);
                self.status = format!(
                    "Loaded {} rows × {} cols in {} ms ({:.0} MB/s) · {:.2} GB resident",
                    fmt_int(stats.rows),
                    stats.cols,
                    stats.parse_millis,
                    stats.throughput_mbps(),
                    sheet.heap_bytes() as f64 / 1e9,
                );
                self.sheet = sheet;
                self.stats = Some(stats);
                self.selection = Some(CellRef::new(0, 0));
                self.loading = false;
                self.load_rx = None;
                // Seed a full-column aggregate so the formula engine is
                // visible and exercised the moment a file opens.
                if self.formula_input.is_empty() && self.sheet.col_count() > 4 {
                    self.formula_input = format!("=SUM(E1:E{})", self.sheet.row_count());
                }
                self.recompute_formula();
            }
            Ok(Err(e)) => {
                self.status = format!("Load failed: {e}");
                self.loading = false;
                self.load_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.status = "Load thread died".into();
                self.loading = false;
                self.load_rx = None;
            }
        }
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv", "tsv", "txt"])
            .pick_file()
        {
            self.start_load(path);
        }
    }

    /// Evaluate whatever is in the formula bar against the live sheet.
    fn recompute_formula(&mut self) {
        let text = self.formula_input.trim();
        if text.is_empty() {
            self.formula_result = None;
            return;
        }
        if !text.starts_with('=') {
            self.formula_result = None;
            return;
        }
        self.formula_result = Some(match parse(text) {
            Ok(expr) => {
                let t = std::time::Instant::now();
                let v = eval(&expr, &self.sheet);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let shown = match v {
                    Value::Number(n) => ferrix_core::format_number(n),
                    Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
                    Value::Text(id) => self.sheet.resolve(id).to_string(),
                    Value::Error(e) => e.to_string(),
                    Value::Empty => String::new(),
                };
                format!("{shown}   ({ms:.1} ms)")
            }
            Err(e) => format!("Parse error: {e}"),
        });
    }

    fn selection_label(&self) -> String {
        self.selection
            .map(|c| c.to_a1())
            .unwrap_or_else(|| "—".into())
    }
}

impl eframe::App for FerrixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_load();
        if self.loading {
            // Keep animating the spinner while the worker parses.
            ctx.request_repaint();
        }

        let frame_start = std::time::Instant::now();

        // --- toolbar ---
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::none().fill(Theme::PANEL).inner_margin(8.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("FERRIX")
                            .color(Theme::ACCENT)
                            .strong()
                            .size(15.0),
                    );
                    ui.add_space(12.0);
                    if ui.button("Open CSV…").clicked() {
                        self.open_dialog();
                    }
                    if self.loading {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new("parsing…").color(Theme::TEXT_DIM));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(s) = &self.stats {
                            ui.label(
                                RichText::new(format!(
                                    "{} rows × {} cols",
                                    fmt_int(s.rows),
                                    s.cols
                                ))
                                .color(Theme::TEXT_DIM)
                                .size(12.0),
                            );
                        }
                    });
                });
            });

        // --- formula bar ---
        egui::TopBottomPanel::top("formula_bar")
            .frame(
                egui::Frame::none()
                    .fill(Theme::HEADER_BG)
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(self.selection_label())
                            .color(Theme::ACCENT)
                            .monospace()
                            .size(13.0),
                    );
                    ui.separator();
                    ui.label(RichText::new("fx").color(Theme::TEXT_DIM).italics());

                    let resp = ui.add_sized(
                        [ui.available_width() * 0.55, 22.0],
                        egui::TextEdit::singleline(&mut self.formula_input)
                            .hint_text("=SUM(E1:E10000000)")
                            .font(egui::TextStyle::Monospace),
                    );
                    // Recompute on edit, and on Enter so a formula restored
                    // from elsewhere (or unchanged) still evaluates.
                    if resp.changed()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        self.recompute_formula();
                    }

                    if let Some(r) = &self.formula_result {
                        let color = if r.starts_with("Parse error") || r.starts_with('#') {
                            Theme::ERROR
                        } else {
                            Theme::NUMBER
                        };
                        ui.label(RichText::new(format!("= {r}")).color(color).monospace());
                    }
                });
            });

        // --- status bar ---
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::none()
                    .fill(Theme::HEADER_BG)
                    .inner_margin(egui::Margin::symmetric(8.0, 4.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&self.status)
                            .color(Theme::TEXT_DIM)
                            .size(11.5),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let fps = if self.frame_ms > 0.0 {
                            1000.0 / self.frame_ms
                        } else {
                            0.0
                        };
                        let color = if fps >= 55.0 {
                            Theme::NUMBER
                        } else if fps >= 30.0 {
                            Theme::TEXT_DIM
                        } else {
                            Theme::ERROR
                        };
                        ui.label(
                            RichText::new(format!(
                                "{:.0} fps · {:.2} ms · {} cells painted",
                                fps, self.frame_ms, self.last_painted
                            ))
                            .color(color)
                            .size(11.5)
                            .monospace(),
                        );
                    });
                });
            });

        // --- grid ---
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Theme::BG))
            .show(ctx, |ui| {
                if self.sheet.row_count() == 0 {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("Open a CSV to get started")
                                .color(Theme::TEXT_DIM)
                                .size(16.0),
                        );
                    });
                    return;
                }

                let resp = Grid {
                    sheet: &self.sheet,
                    selection: self.selection,
                    col_widths: &self.col_widths,
                }
                .show(ui);

                self.last_painted = resp.painted_cells;

                if let Some(cell) = resp.clicked {
                    self.selection = Some(cell);
                    // Clicking a cell seeds the formula bar with its A1 ref so
                    // building a formula from selections is quick.
                    if self.formula_input.is_empty() {
                        self.formula_input = format!("={}", cell.to_a1());
                        self.recompute_formula();
                    }
                }
            });

        // Exponential moving average keeps the readout stable but responsive.
        let ms = frame_start.elapsed().as_secs_f64() as f32 * 1000.0;
        self.frame_ms = if self.frame_ms == 0.0 {
            ms
        } else {
            self.frame_ms * 0.9 + ms * 0.1
        };
    }
}

/// Size each column from a sample of its contents. We sample rather than scan
/// so opening a 10M-row file stays instant.
fn compute_col_widths(sheet: &Sheet) -> Vec<f32> {
    const SAMPLE: usize = 200;
    let rows = sheet.row_count();
    let step = (rows / SAMPLE).max(1);

    (0..sheet.col_count())
        .map(|c| {
            let mut widest = sheet.header_or_letter(c).len();
            let mut r = 0;
            while r < rows {
                let len = sheet.display(CellRef::new(r as u32, c as u32)).len();
                widest = widest.max(len);
                r += step;
            }
            // ~7px per character plus padding, clamped to sane bounds.
            (widest as f32 * 7.2 + 20.0).clamp(64.0, 320.0)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|w| if w.is_finite() { w } else { DEFAULT_COL_WIDTH })
        .collect()
}

/// Thousands separators for readability in the status bar.
fn fmt_int(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_formatting() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1000), "1,000");
        assert_eq!(fmt_int(10_000_000), "10,000,000");
    }

    #[test]
    fn col_widths_are_bounded_and_sampled() {
        let mut s = Sheet::new("t");
        for r in 0..1000u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
            s.set_text(CellRef::new(r, 1), "a-fairly-long-text-value-here");
        }
        let w = compute_col_widths(&s);
        assert_eq!(w.len(), 2);
        for width in &w {
            assert!(
                (64.0..=320.0).contains(width),
                "width {width} outside clamp range"
            );
        }
        // The text column must come out wider than the numeric one.
        assert!(w[1] > w[0]);
    }
}
