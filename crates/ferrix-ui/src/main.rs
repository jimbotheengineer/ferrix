//! Ferrix — a fast, open-source spreadsheet.

mod app;
mod chart_panel;
mod grid;
#[cfg(test)]
mod harness;
mod sheet_view;
mod table_view;
mod theme;
mod workbook;

use app::FerrixApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 880.0])
            .with_min_inner_size([720.0, 420.0])
            .with_title("Ferrix"),
        ..Default::default()
    };

    // A file passed on the command line loads at startup.
    let initial = std::env::args().nth(1).map(std::path::PathBuf::from);

    eframe::run_native(
        "Ferrix",
        options,
        Box::new(move |cc| {
            theme::Theme::apply(&cc.egui_ctx);
            Ok(Box::new(FerrixApp::new(initial)))
        }),
    )
}
