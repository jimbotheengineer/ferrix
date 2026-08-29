//! Writes a sample PDF and HTML so an external tool can check them.
//!
//! The unit tests parse the PDF with an in-crate reader. That proves the file
//! is self-consistent, but not that a *third party* accepts it — and a writer
//! and its matching reader can share the same wrong assumption. This example
//! exists so `pdftotext` (poppler) can be pointed at real output.
//!
//! Run: `cargo run -p ferrix-io --example print_sample -- <outdir>`

use std::collections::HashMap;

use ferrix_core::page::{FieldContext, Margins, PageSetup, PaperSize};
use ferrix_core::{CellRef, ColSizes, HAlign, Rgb, RowSizes, TableRange};
use ferrix_io::export::ExportSource;
use ferrix_io::render::{render_html, render_pdf, CellPaint, RenderOptions, RenderSource};

struct Demo {
    rows: usize,
    cols: usize,
    values: HashMap<(u32, u32), String>,
}

impl ExportSource for Demo {
    fn row_count(&self) -> usize {
        self.rows
    }
    fn col_count(&self) -> usize {
        self.cols
    }
    fn display(&self, cell: CellRef) -> String {
        if let Some(v) = self.values.get(&(cell.row, cell.col)) {
            return v.clone();
        }
        if cell.col == 0 {
            format!("ROW{}", cell.row)
        } else {
            format!("{}.{:02}", cell.row * 10 + cell.col, cell.col * 7 % 100)
        }
    }
    fn header(&self, col: usize) -> String {
        format!("Col{col}")
    }
}

impl RenderSource for Demo {
    fn paint(&self, cell: CellRef) -> CellPaint {
        if cell.row == 0 {
            return CellPaint {
                fill: Some(Rgb(220, 230, 245)),
                bold: true,
                align: HAlign::Center,
                ..Default::default()
            };
        }
        if cell.row % 7 == 0 && cell.col == 2 {
            return CellPaint {
                fill: Some(Rgb(255, 220, 220)),
                text_color: Some(Rgb(160, 0, 0)),
                ..Default::default()
            };
        }
        CellPaint::default()
    }
    fn merge_at(&self, cell: CellRef) -> Option<TableRange> {
        let m = TableRange::new(1, 1, 1, 3);
        (cell.row >= m.first_row
            && cell.row <= m.last_row
            && cell.col >= m.first_col
            && cell.col <= m.last_col)
            .then_some(m)
    }
    fn sheet_name(&self) -> String {
        "Regional Summary".into()
    }
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let mut values = HashMap::new();
    values.insert((1, 1), "MERGED HEADING".to_string());
    values.insert((0, 0), "Region".to_string());
    values.insert((0, 1), "Q1".to_string());
    values.insert((0, 2), "Q2".to_string());
    values.insert((0, 3), "Total".to_string());
    values.insert((5, 0), "Sentinel-Page1".to_string());
    values.insert((120, 0), "Sentinel-Later".to_string());

    let sheet = Demo {
        rows: 220,
        cols: 5,
        values,
    };
    let mut setup = PageSetup {
        paper: PaperSize::Letter,
        margins: Margins::default(),
        gridlines: true,
        ..PageSetup::default()
    };
    setup.header.center = "Regional Summary".into();
    setup.header.right = "Page &P of &N".into();
    setup.footer.left = "&F".into();
    setup.repeat_rows = Some((0, 0));

    let opts = RenderOptions {
        fields: FieldContext {
            file: "sample.fxs".into(),
            date: "2026-08-29".into(),
            time: "18:30".into(),
            ..FieldContext::default()
        },
        ..RenderOptions::default()
    };
    let (rows, cols) = (RowSizes::default(), ColSizes::default());

    let pdf = std::path::Path::new(&dir).join("sample.pdf");
    let stats = render_pdf(
        &pdf,
        &sheet,
        &setup,
        &opts,
        &rows,
        &cols,
        true,
        |_, _| {},
        || false,
    )
    .expect("pdf render failed");
    println!(
        "pdf  {} pages, {} bytes -> {}",
        stats.pages,
        stats.bytes,
        pdf.display()
    );

    let html = std::path::Path::new(&dir).join("sample.html");
    let stats = render_html(
        &html,
        &sheet,
        &setup,
        &opts,
        &rows,
        &cols,
        true,
        |_, _| {},
        || false,
    )
    .expect("html render failed");
    println!(
        "html {} pages, {} bytes -> {}",
        stats.pages,
        stats.bytes,
        html.display()
    );
}
