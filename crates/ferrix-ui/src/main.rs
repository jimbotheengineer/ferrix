//! Ferrix — a fast, open-source spreadsheet.

mod agent_bridge;
mod app;
mod chart_panel;
mod command;
mod cond_format;
mod grid;
#[cfg(test)]
mod harness;
mod import_wizard;
mod page_setup_dialog;
mod pivot_builder;
mod prefs;
mod protect_panel;
mod recent;
mod selection_panel;
mod sheet_view;
mod table_view;
mod theme;
mod trace;
mod validation_panel;
mod workbook;

use app::FerrixApp;

/// `FERRIX_WINDOW=1280x900` overrides the launch window size — used by the
/// visual-verification pass to capture the app at specific resolutions.
fn window_size_from_env() -> Option<[f32; 2]> {
    let spec = std::env::var("FERRIX_WINDOW").ok()?;
    let (w, h) = spec.split_once(['x', 'X'])?;
    Some([w.trim().parse().ok()?, h.trim().parse().ok()?])
}

fn main() -> eframe::Result<()> {
    // Size the worker pool BEFORE anything can touch rayon: the first
    // `par_iter` in the process builds a default all-cores pool implicitly,
    // and `build_global` can only be called once. Doing it here is what keeps
    // the machine usable during a multi-minute conversion.
    let threads = ferrix_io::pool::init();
    eprintln!(
        "ferrix: {} · {}",
        ferrix_io::pool::describe(),
        ferrix_core::Budget::sample().describe()
    );
    debug_assert!(threads >= 1);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(window_size_from_env().unwrap_or([1400.0, 880.0]))
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
            // The platform type ramp (Segoe UI / Cascadia Mono on Windows)
            // installs once; the palette is re-applied every frame by the app.
            theme::install_fonts(&cc.egui_ctx);
            // The app owns the palette from here on and re-applies it every
            // frame (it may follow the OS on the first one, or be toggled).
            // This first call just avoids a single unstyled frame.
            let app = FerrixApp::new(initial);
            app.theme().apply(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}
