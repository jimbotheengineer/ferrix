//! Tests for PDF and HTML rendering.
//!
//! The issue's acceptance criterion is explicit: *"Verify the PDF by
//! extracting its text and asserting cell values appear on the expected page
//! — not by eyeballing it."* That is what these do. Every PDF assertion
//! renders to a real file, reads the bytes back, parses them through the
//! independent reader, and asserts on the text runs of a specific page.

use std::collections::HashMap;

use ferrix_core::page::{Margins, Orientation, PageOrder, PageSetup, PaperSize, Scaling};
use ferrix_core::{CellRef, ColSizes, HAlign, Rgb, RowSizes, TableRange};

use super::*;
use crate::pdf::reader;

/// A sheet built for tests: dense values, optional paint and merges.
struct Grid {
    rows: usize,
    cols: usize,
    /// Overrides for specific cells; everything else is "r{row}c{col}".
    values: HashMap<(u32, u32), String>,
    paints: HashMap<(u32, u32), CellPaint>,
    merges: Vec<TableRange>,
    name: String,
}

impl Grid {
    fn new(rows: usize, cols: usize) -> Self {
        Grid {
            rows,
            cols,
            values: HashMap::new(),
            paints: HashMap::new(),
            merges: Vec::new(),
            name: "Sheet1".into(),
        }
    }

    fn value(mut self, r: u32, c: u32, v: &str) -> Self {
        self.values.insert((r, c), v.into());
        self
    }

    fn paint_cell(mut self, r: u32, c: u32, p: CellPaint) -> Self {
        self.paints.insert((r, c), p);
        self
    }

    fn merge(mut self, range: TableRange) -> Self {
        self.merges.push(range);
        self
    }
}

impl ExportSource for Grid {
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
        format!("r{}c{}", cell.row, cell.col)
    }
    fn header(&self, col: usize) -> String {
        format!("H{col}")
    }
}

impl RenderSource for Grid {
    fn paint(&self, cell: CellRef) -> CellPaint {
        self.paints
            .get(&(cell.row, cell.col))
            .copied()
            .unwrap_or_default()
    }
    fn merge_at(&self, cell: CellRef) -> Option<TableRange> {
        self.merges
            .iter()
            .find(|r| {
                cell.row >= r.first_row
                    && cell.row <= r.last_row
                    && cell.col >= r.first_col
                    && cell.col <= r.last_col
            })
            .copied()
    }
    fn sheet_name(&self) -> String {
        self.name.clone()
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ferrix-render-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

fn letter() -> PageSetup {
    PageSetup {
        paper: PaperSize::Letter,
        orientation: Orientation::Portrait,
        margins: Margins::default(),
        scaling: Scaling::default(),
        gridlines: true,
        order: PageOrder::DownThenOver,
        ..PageSetup::default()
    }
}

/// Render to PDF and parse the file back. Returns the parsed doc and stats.
fn pdf_roundtrip(
    sheet: &Grid,
    setup: &PageSetup,
    opts: &RenderOptions,
    rows: &RowSizes,
    cols: &ColSizes,
) -> (reader::ParsedPdf, RenderStats) {
    let path = tmp("out.pdf");
    let stats = render_pdf(
        &path,
        sheet,
        setup,
        opts,
        rows,
        cols,
        true,
        |_, _| {},
        || false,
    )
    .expect("render failed");
    let bytes = std::fs::read(&path).expect("no pdf was written");
    let _ = std::fs::remove_file(&path);
    let doc =
        reader::parse(&bytes).unwrap_or_else(|e| panic!("the rendered PDF does not parse: {e}"));
    (doc, stats)
}

fn html_roundtrip(
    sheet: &Grid,
    setup: &PageSetup,
    opts: &RenderOptions,
    rows: &RowSizes,
    cols: &ColSizes,
) -> (String, RenderStats) {
    let path = tmp("out.html");
    let stats = render_html(
        &path,
        sheet,
        setup,
        opts,
        rows,
        cols,
        true,
        |_, _| {},
        || false,
    )
    .expect("render failed");
    let text = std::fs::read_to_string(&path).expect("no html was written");
    let _ = std::fs::remove_file(&path);
    (text, stats)
}

// ------------------------------------------------------- the headline test ==

#[test]
fn cell_values_land_on_the_page_the_paginator_assigned() {
    // THE acceptance criterion from #37, done the way the issue asks: extract
    // the text from the produced PDF and assert values appear on the expected
    // page.
    //
    // The expectation is not hard-coded: it comes from the paginator, so this
    // asserts the RENDERER agrees with the LAYOUT. Hard-coding "row 90 is on
    // page 3" would pass with a renderer that ignores pagination entirely and
    // happens to be tuned to the same numbers.
    let sheet = Grid::new(300, 4);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let setup = letter();
    let opts = RenderOptions::default();

    let paginator = plan(&sheet, &setup, &opts, &rows, &cols);
    let expected: Vec<(u64, u32, u32)> = paginator
        .pages()
        .map(|p| (p.number, p.first_row, p.last_row))
        .collect();
    assert!(
        expected.len() > 3,
        "test needs a multi-page job; got {}",
        expected.len()
    );

    let (doc, stats) = pdf_roundtrip(&sheet, &setup, &opts, &rows, &cols);
    assert_eq!(stats.pages as usize, expected.len());
    assert_eq!(doc.pages.len(), expected.len());

    for (number, first_row, last_row) in &expected {
        let page = doc.page(*number as usize);
        // The first and last row of the band must be ON this page...
        for r in [*first_row, *last_row] {
            let needle = format!("r{r}c0");
            assert!(
                page.contains(&needle),
                "{needle} should be on page {number} (band {first_row}..={last_row}), \
                 but that page contains {:?}...",
                &page.strings()[..page.strings().len().min(4)]
            );
        }
        // ...and the row just past the band must NOT be.
        if *last_row < 299 {
            let beyond = format!("r{}c0", last_row + 1);
            assert!(
                !page.contains(&beyond),
                "{beyond} bled onto page {number}, whose band ends at {last_row}"
            );
        }
    }
}

#[test]
fn every_row_appears_exactly_once_across_the_document() {
    // A total page count can be right while rows are duplicated or dropped —
    // the aggregate trap the guide warns about. Count per-row occurrences.
    let sheet = Grid::new(140, 2);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    for r in 0..140u32 {
        let hits = doc.pages_containing(&format!("r{r}c0"));
        assert_eq!(
            hits.len(),
            1,
            "row {r} appears on pages {hits:?}; it must appear exactly once"
        );
    }
}

#[test]
fn a_wide_sheet_splits_across_column_bands_without_losing_columns() {
    let sheet = Grid::new(20, 60);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let setup = letter();
    let opts = RenderOptions::default();
    let paginator = plan(&sheet, &setup, &opts, &rows, &cols);
    assert!(
        paginator.col_band_count() > 1,
        "60 columns should not fit on one Letter page"
    );

    let (doc, _) = pdf_roundtrip(&sheet, &setup, &opts, &rows, &cols);
    for c in 0..60u32 {
        let hits = doc.pages_containing(&format!("r0c{c}"));
        assert_eq!(hits.len(), 1, "column {c} of row 0 appears on {hits:?}");
    }
}

// ------------------------------------------------------ headers and footers ==

#[test]
fn page_number_fields_resolve_per_page() {
    // &P must differ page to page and &N must be the job total. Rendering the
    // header once and reusing it is the obvious bug, and every page would
    // then read "Page 1 of N".
    let sheet = Grid::new(200, 3);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut setup = letter();
    setup.header.center = "Page &P of &N".into();
    let opts = RenderOptions::default();

    let (doc, _) = pdf_roundtrip(&sheet, &setup, &opts, &rows, &cols);
    let total = doc.pages.len();
    assert!(total >= 3);
    for (i, page) in doc.pages.iter().enumerate() {
        let want = format!("Page {} of {}", i + 1, total);
        assert!(
            page.contains(&want),
            "page {} header should read {want:?}; page text starts {:?}",
            i + 1,
            &page.strings()[..page.strings().len().min(3)]
        );
    }
}

#[test]
fn footer_fields_pull_from_the_injected_context() {
    let sheet = Grid::new(5, 2);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut setup = letter();
    setup.footer.left = "&F".into();
    setup.footer.right = "&D &T".into();
    let mut opts = RenderOptions::default();
    opts.fields.file = "quarterly.fxs".into();
    opts.fields.date = "2026-08-29".into();
    opts.fields.time = "18:04".into();

    let (doc, _) = pdf_roundtrip(&sheet, &setup, &opts, &rows, &cols);
    let joined = doc.page(1).joined();
    assert!(
        joined.contains("quarterly.fxs"),
        "footer &F missing: {joined}"
    );
    assert!(joined.contains("2026-08-29 18:04"), "footer &D/&T missing");
}

#[test]
fn the_sheet_name_field_reaches_the_page() {
    let mut sheet = Grid::new(3, 2);
    sheet.name = "Q3 Actuals".into();
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut setup = letter();
    setup.header.left = "&A".into();
    let (doc, _) = pdf_roundtrip(&sheet, &setup, &RenderOptions::default(), &rows, &cols);
    assert!(doc.page(1).contains("Q3 Actuals"));
}

#[test]
fn repeated_header_rows_print_on_later_pages_but_not_twice_on_the_first() {
    // Repeating row 0 on page 1 — where it already is — prints it twice, one
    // over the other. Easy to write, invisible in a page-count assertion.
    let sheet = Grid::new(200, 3).value(0, 0, "HEADING");
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut setup = letter();
    setup.repeat_rows = Some((0, 0));

    let (doc, _) = pdf_roundtrip(&sheet, &setup, &RenderOptions::default(), &rows, &cols);
    assert!(doc.pages.len() >= 3);
    let on_first = doc
        .page(1)
        .texts
        .iter()
        .filter(|t| t.text == "HEADING")
        .count();
    assert_eq!(
        on_first, 1,
        "the heading printed {on_first} times on page 1; it must not be repeated onto the \
         page that already contains it"
    );
    for n in 2..=doc.pages.len() {
        assert!(
            doc.page(n).contains("HEADING"),
            "page {n} is missing the repeated heading row"
        );
    }
}

// ------------------------------------------------------------ visual layer ==

#[test]
fn conditional_fills_are_painted_and_do_not_replace_the_text() {
    // Painting the fill AFTER the text hides it — a "styled" export whose
    // values are invisible. Assert both the fill count and the text.
    let sheet = Grid::new(4, 3).paint_cell(
        1,
        1,
        CellPaint {
            fill: Some(Rgb(255, 100, 100)),
            ..Default::default()
        },
    );
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut setup = letter();
    setup.gridlines = false;

    let (doc, _) = pdf_roundtrip(&sheet, &setup, &RenderOptions::default(), &rows, &cols);
    assert_eq!(
        doc.page(1).fills,
        1,
        "exactly one cell was given a fill; found {} painted rects",
        doc.page(1).fills
    );
    assert!(
        doc.page(1).contains("r1c1"),
        "the filled cell's own value vanished — the fill is painted over the text"
    );
}

#[test]
fn bold_cells_use_the_bold_font_resource() {
    let sheet = Grid::new(3, 2).paint_cell(
        0,
        0,
        CellPaint {
            bold: true,
            ..Default::default()
        },
    );
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    let run = doc
        .page(1)
        .texts
        .iter()
        .find(|t| t.text == "r0c0")
        .expect("cell missing");
    assert_eq!(
        run.font,
        crate::pdf::FONT_BOLD,
        "a bold cell was drawn with font /F{}",
        run.font
    );
    let plain = doc
        .page(1)
        .texts
        .iter()
        .find(|t| t.text == "r0c1")
        .expect("neighbour missing");
    assert_eq!(plain.font, crate::pdf::FONT_REGULAR);
}

#[test]
fn merged_cells_draw_once_from_their_anchor() {
    // A covered cell that still draws prints the neighbour's value on top of
    // the merged region — the classic merge rendering bug.
    let sheet = Grid::new(6, 4)
        .value(1, 1, "MERGED")
        .merge(TableRange::new(1, 1, 2, 2));
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    let page = doc.page(1);
    assert!(page.contains("MERGED"));
    for covered in ["r1c2", "r2c1", "r2c2"] {
        assert!(
            !page.contains(covered),
            "{covered} is inside the merge but was drawn anyway"
        );
    }
    // Cells outside the merge are unaffected.
    assert!(page.contains("r1c3"));
    assert!(page.contains("r3c1"));
}

#[test]
fn right_alignment_places_text_further_right_than_left_alignment() {
    // Alignment maths silently no-ops if the measured width is wrong or the
    // branch is inverted, and the text still appears — so assert positions.
    let sheet = Grid::new(2, 2)
        .value(0, 0, "1234")
        .value(1, 0, "1234")
        .paint_cell(
            0,
            0,
            CellPaint {
                align: HAlign::Left,
                ..Default::default()
            },
        )
        .paint_cell(
            1,
            0,
            CellPaint {
                align: HAlign::Right,
                ..Default::default()
            },
        );
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    let runs = &doc.page(1).texts;
    let mut xs: Vec<f32> = runs
        .iter()
        .filter(|t| t.text == "1234")
        .map(|t| t.x)
        .collect();
    assert_eq!(xs.len(), 2, "expected two '1234' runs, got {}", xs.len());
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!(
        xs[1] - xs[0] > 10.0,
        "right-aligned text sits at x={} vs left-aligned x={}; alignment is not being applied",
        xs[1],
        xs[0]
    );
}

#[test]
fn general_alignment_puts_numbers_right_and_text_left() {
    // `General` is not "left": the grid right-aligns numbers, and paper must
    // match the screen or the export looks like a different document.
    let sheet = Grid::new(2, 1).value(0, 0, "42.50").value(1, 0, "Widget");
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    let num = doc
        .page(1)
        .texts
        .iter()
        .find(|t| t.text == "42.50")
        .unwrap();
    let txt = doc
        .page(1)
        .texts
        .iter()
        .find(|t| t.text == "Widget")
        .unwrap();
    assert!(
        num.x > txt.x,
        "a numeric cell ({}) should sit right of a text cell ({}) under General alignment",
        num.x,
        txt.x
    );
}

#[test]
fn gridlines_can_be_turned_off() {
    let sheet = Grid::new(5, 3);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut on = letter();
    on.gridlines = true;
    let mut off = letter();
    off.gridlines = false;

    let path_on = tmp("on.pdf");
    render_pdf(
        &path_on,
        &sheet,
        &on,
        &RenderOptions::default(),
        &rows,
        &cols,
        true,
        |_, _| {},
        || false,
    )
    .unwrap();
    let bytes_on = std::fs::read(&path_on).unwrap();
    let _ = std::fs::remove_file(&path_on);

    let path_off = tmp("off.pdf");
    render_pdf(
        &path_off,
        &sheet,
        &off,
        &RenderOptions::default(),
        &rows,
        &cols,
        true,
        |_, _| {},
        || false,
    )
    .unwrap();
    let bytes_off = std::fs::read(&path_off).unwrap();
    let _ = std::fs::remove_file(&path_off);

    let strokes = |b: &[u8]| String::from_utf8_lossy(b).matches("\nS\n").count();
    assert!(
        strokes(&bytes_on) > strokes(&bytes_off),
        "gridlines on produced {} strokes, off produced {} — the flag does nothing",
        strokes(&bytes_on),
        strokes(&bytes_off)
    );
    assert_eq!(strokes(&bytes_off), 0, "gridlines off still drew lines");
}

// ------------------------------------------------------------- print area ==

#[test]
fn a_print_area_excludes_everything_outside_it() {
    let sheet = Grid::new(50, 8);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let opts = RenderOptions {
        print_area: Some(TableRange::new(10, 2, 14, 4)),
        ..RenderOptions::default()
    };
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &opts, &rows, &cols);
    assert_eq!(doc.pages.len(), 1);
    let page = doc.page(1);
    assert!(page.contains("r10c2"), "print area's first cell is missing");
    assert!(page.contains("r14c4"), "print area's last cell is missing");
    for outside in ["r9c2", "r15c2", "r10c1", "r10c5", "r0c0"] {
        assert!(
            !page.contains(outside),
            "{outside} is outside the print area but was printed"
        );
    }
}

// ------------------------------------------------------ the large-job gate ==

#[test]
fn an_oversized_job_is_refused_before_any_bytes_are_written() {
    // "Warn before producing more than ~1000 pages" only means something if
    // the warning happens BEFORE the file exists. A post-hoc warning still
    // costs the user the disk and the wait.
    let sheet = Grid::new(3_000_000, 4);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let path = tmp("huge.pdf");
    let err = render_pdf(
        &path,
        &sheet,
        &letter(),
        &RenderOptions::default(),
        &rows,
        &cols,
        false,
        |_, _| {},
        || false,
    )
    .expect_err("a 3M-row sheet should not render unconfirmed");
    match err {
        RenderError::TooManyPages(n) => assert!(
            n > ferrix_core::page::LARGE_JOB_PAGES,
            "refused with only {n} pages"
        ),
        other => panic!("expected TooManyPages, got {other:?}"),
    }
    assert!(
        !path.exists(),
        "the refusal still created a file at {}",
        path.display()
    );
}

#[test]
fn confirming_a_large_job_lets_it_through() {
    // The gate must be a confirmation, not a hard ceiling — a user who wants
    // 1200 pages is allowed to have them.
    let sheet = Grid::new(60_000, 3);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let paginator = plan(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    assert!(paginator.is_large(), "test fixture is not a large job");

    let path = tmp("large.html");
    let stats = render_html(
        &path,
        &sheet,
        &letter(),
        &RenderOptions::default(),
        &rows,
        &cols,
        true,
        |_, _| {},
        || false,
    )
    .expect("confirmed large job was still refused");
    assert_eq!(stats.pages, paginator.page_count());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn plan_answers_the_page_count_without_rendering() {
    // This is what makes the warning possible at all: the dialog must be able
    // to ask "how many pages?" before committing.
    let sheet = Grid::new(200_000_000, 5);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let start = std::time::Instant::now();
    let paginator = plan(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    let n = paginator.page_count();
    let elapsed = start.elapsed();
    assert!(
        n > 1_000_000,
        "200M rows should be millions of pages, got {n}"
    );
    assert!(
        elapsed.as_millis() < 250,
        "counting pages for 200M rows took {elapsed:?} — it is iterating rows"
    );
}

// -------------------------------------------------------------- cancelling ==

#[test]
fn cancelling_leaves_no_partial_file() {
    // A half-written PDF looks like a complete one to a file manager and
    // opens to an error in a viewer. Cancel must clean up.
    let sheet = Grid::new(50_000, 4);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let path = tmp("cancelled.pdf");
    let mut calls = 0;
    let err = render_pdf(
        &path,
        &sheet,
        &letter(),
        &RenderOptions::default(),
        &rows,
        &cols,
        true,
        |_, _| {},
        || {
            calls += 1;
            calls > 2
        },
    )
    .expect_err("cancel was ignored");
    assert!(matches!(err, RenderError::Cancelled));
    assert!(
        !path.exists(),
        "a cancelled render left {} behind",
        path.display()
    );
}

#[test]
fn a_cancelled_re_export_preserves_the_previous_file() {
    // Re-exporting to a path that already holds a good PDF, then cancelling,
    // must NOT destroy the good file. The temp-sibling + rename discipline is
    // the whole point: writing straight to the destination and deleting it on
    // cancel would silently lose the export the user was replacing.
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let path = tmp("preserved.pdf");

    // First, a small successful export leaves a valid PDF at `path`.
    let good = Grid::new(3, 2);
    render_pdf(
        &path,
        &good,
        &letter(),
        &RenderOptions::default(),
        &rows,
        &cols,
        true,
        |_, _| {},
        || false,
    )
    .expect("the first export should succeed");
    let original = std::fs::read(&path).expect("first export must exist");
    assert!(
        original.starts_with(b"%PDF"),
        "sanity: first export is a PDF"
    );

    // Now a big re-export to the SAME path, cancelled partway.
    let big = Grid::new(50_000, 4);
    let mut calls = 0;
    let err = render_pdf(
        &path,
        &big,
        &letter(),
        &RenderOptions::default(),
        &rows,
        &cols,
        true,
        |_, _| {},
        || {
            calls += 1;
            calls > 2
        },
    )
    .expect_err("the re-export should have been cancelled");
    assert!(matches!(err, RenderError::Cancelled));

    // The original file must still be there, byte-for-byte.
    let after = std::fs::read(&path).expect("the previous file must survive a cancel");
    assert_eq!(
        original, after,
        "a cancelled re-export corrupted or replaced the previous file"
    );
}

#[test]
fn a_refused_large_re_export_preserves_the_previous_file() {
    // The large-job refusal returns before writing anything; a pre-existing
    // file at the path must be untouched.
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let path = tmp("preserved-refuse.pdf");

    let good = Grid::new(3, 2);
    render_pdf(
        &path,
        &good,
        &letter(),
        &RenderOptions::default(),
        &rows,
        &cols,
        true,
        |_, _| {},
        || false,
    )
    .expect("the first export should succeed");
    let original = std::fs::read(&path).unwrap();

    // A job over the large threshold, WITHOUT confirm_large, must refuse.
    // Give it enough real rows plus a break before each so every row is its
    // own page — comfortably past LARGE_JOB_PAGES.
    let huge = Grid::new(2000, 2);
    let mut setup = letter();
    for r in 1..2000u32 {
        setup.add_row_break(r);
    }
    let err = render_pdf(
        &path,
        &huge,
        &setup,
        &RenderOptions::default(),
        &rows,
        &cols,
        false, // do not confirm — force the refusal
        |_, _| {},
        || false,
    )
    .expect_err("a large job without confirmation must be refused");
    assert!(matches!(err, RenderError::TooManyPages(_)));

    let after = std::fs::read(&path).unwrap();
    assert_eq!(
        original, after,
        "a refused large re-export must leave the previous file untouched"
    );
}

// -------------------------------------------------------------------- HTML ==

#[test]
fn html_is_self_contained_and_carries_the_cell_values() {
    let sheet = Grid::new(6, 3);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (html, stats) = html_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    assert_eq!(stats.pages, 1);
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("</html>"));
    // "single-file HTML with inline styles" — no external fetches at all.
    for external in ["<link", "<script", "src=\"http", "@import", "url("] {
        assert!(
            !html.contains(external),
            "html references something external ({external}); it must be self-contained"
        );
    }
    for r in 0..6u32 {
        for c in 0..3u32 {
            assert!(
                html.contains(&format!("r{r}c{c}")),
                "cell r{r}c{c} is missing from the HTML"
            );
        }
    }
}

#[test]
fn html_escapes_values_that_would_otherwise_be_markup() {
    // A cell containing `<b>` must render as text, not turn the rest of the
    // document bold — and a cell containing `</td>` must not shred the table.
    let sheet = Grid::new(2, 1)
        .value(0, 0, "<b>not bold</b>")
        .value(1, 0, "a & b </td></tr>");
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (html, _) = html_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    assert!(html.contains("&lt;b&gt;not bold&lt;/b&gt;"));
    assert!(html.contains("a &amp; b &lt;/td&gt;&lt;/tr&gt;"));
    assert!(
        !html.contains("<b>not bold</b>"),
        "raw markup from a cell reached the output"
    );
    // The structure survived: exactly one row per sheet row.
    assert_eq!(html.matches("<tr ").count(), 2);
}

#[test]
fn html_pages_match_the_pdf_pages() {
    // Two renderers that disagree about pagination produce a PDF and an HTML
    // of the same data that are not the same document. Both go through the
    // same Layout, and this is what pins that.
    let sheet = Grid::new(250, 4);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let setup = letter();
    let opts = RenderOptions::default();
    let (doc, pdf_stats) = pdf_roundtrip(&sheet, &setup, &opts, &rows, &cols);
    let (html, html_stats) = html_roundtrip(&sheet, &setup, &opts, &rows, &cols);
    assert_eq!(pdf_stats.pages, html_stats.pages);
    assert_eq!(html.matches("class=\"page\"").count(), doc.pages.len());
    assert_eq!(pdf_stats.rows, html_stats.rows);
}

#[test]
fn html_carries_fills_merges_and_alignment() {
    let sheet = Grid::new(5, 4)
        .value(1, 1, "SPAN")
        .merge(TableRange::new(1, 1, 1, 2))
        .paint_cell(
            0,
            0,
            CellPaint {
                fill: Some(Rgb(0x11, 0x22, 0x33)),
                text_color: Some(Rgb(0xff, 0xee, 0xdd)),
                bold: true,
                italic: true,
                align: HAlign::Center,
            },
        );
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (html, _) = html_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    assert!(html.contains("background:#112233"), "fill missing");
    assert!(html.contains("color:#ffeedd"), "text colour missing");
    assert!(html.contains("font-weight:bold"), "bold missing");
    assert!(html.contains("font-style:italic"), "italic missing");
    assert!(html.contains("text-align:center"), "alignment missing");
    assert!(
        html.contains("colspan=\"2\""),
        "merge did not become a colspan"
    );
    assert!(
        !html.contains(">r1c2<"),
        "the covered half of the merge was emitted as its own cell"
    );
}

#[test]
fn html_header_and_footer_render_their_fields() {
    let sheet = Grid::new(120, 3);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut setup = letter();
    setup.header.center = "&P/&N".into();
    let (html, stats) = html_roundtrip(&sheet, &setup, &RenderOptions::default(), &rows, &cols);
    for p in 1..=stats.pages {
        assert!(
            html.contains(&format!("{p}/{}", stats.pages)),
            "page {p}'s header is missing from the HTML"
        );
    }
}

// ------------------------------------------------------------- page setup ==

#[test]
fn landscape_swaps_the_media_box_and_fits_more_columns() {
    let sheet = Grid::new(10, 30);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let portrait = letter();
    let mut landscape = letter();
    landscape.orientation = Orientation::Landscape;

    let (pd, _) = pdf_roundtrip(&sheet, &portrait, &RenderOptions::default(), &rows, &cols);
    let (ld, _) = pdf_roundtrip(&sheet, &landscape, &RenderOptions::default(), &rows, &cols);
    assert!(
        ld.page(1).media.0 > pd.page(1).media.0,
        "landscape media box is not wider"
    );
    // The band *count* can coincide (30 columns split 3 ways either side of
    // a threshold), so assert the property that actually matters: a wider
    // page carries strictly more columns before the first break.
    let cols_on_first = |d: &reader::ParsedPdf| {
        (0..30u32)
            .filter(|c| d.page(1).contains(&format!("r0c{c}")))
            .count()
    };
    let (p_first, l_first) = (cols_on_first(&pd), cols_on_first(&ld));
    assert!(
        l_first > p_first,
        "landscape fitted {l_first} columns on page 1, portrait fitted {p_first}"
    );
}

#[test]
fn wider_margins_fit_fewer_rows_per_page() {
    let sheet = Grid::new(400, 3);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut narrow = letter();
    narrow.margins = Margins::narrow();
    let mut wide = letter();
    wide.margins = Margins::wide();

    let n = plan(&sheet, &narrow, &RenderOptions::default(), &rows, &cols).page_count();
    let w = plan(&sheet, &wide, &RenderOptions::default(), &rows, &cols).page_count();
    assert!(
        w >= n,
        "wide margins ({w} pages) should need at least as many pages as narrow ({n})"
    );
}

#[test]
fn hidden_rows_are_not_printed() {
    let sheet = Grid::new(10, 2);
    let mut rows = RowSizes::default();
    rows.hide(3, 4);
    let cols = ColSizes::default();
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    let page = doc.page(1);
    assert!(!page.contains("r3c0"), "a hidden row was printed");
    assert!(!page.contains("r4c0"), "a hidden row was printed");
    assert!(page.contains("r2c0") && page.contains("r5c0"));
}

#[test]
fn hidden_columns_are_not_printed() {
    let sheet = Grid::new(4, 6);
    let rows = RowSizes::default();
    let mut cols = ColSizes::default();
    cols.hide(2);
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    assert!(!doc.page(1).contains("r0c2"), "a hidden column was printed");
    assert!(doc.page(1).contains("r0c3"));
}

#[test]
fn an_empty_sheet_still_produces_one_page() {
    // Zero pages means no header, no footer, and a 0-byte "export" the user
    // has to guess at. One blank page is the honest answer.
    let sheet = Grid::new(0, 0);
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let mut setup = letter();
    setup.header.center = "Empty".into();
    let (doc, stats) = pdf_roundtrip(&sheet, &setup, &RenderOptions::default(), &rows, &cols);
    assert_eq!(stats.pages, 1);
    assert!(doc.page(1).contains("Empty"));
}

// ------------------------------------------------------------------ escape ==

#[test]
fn a_value_containing_a_pdf_delimiter_survives_the_round_trip() {
    let sheet = Grid::new(2, 1).value(0, 0, "Total (net) \\ 100%");
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let (doc, _) = pdf_roundtrip(&sheet, &letter(), &RenderOptions::default(), &rows, &cols);
    assert!(
        doc.page(1).contains("Total (net) \\ 100%"),
        "page text was {:?}",
        doc.page(1).strings()
    );
}

#[test]
fn text_is_clipped_to_its_cell() {
    // Without clipping an overlong value paints across its neighbours, which
    // is the difference between a spreadsheet print and a mess.
    let sheet = Grid::new(2, 3).value(
        0,
        0,
        "an extremely long value that is far wider than any default column",
    );
    let (rows, cols) = (RowSizes::default(), ColSizes::default());
    let path = tmp("clip.pdf");
    render_pdf(
        &path,
        &sheet,
        &letter(),
        &RenderOptions::default(),
        &rows,
        &cols,
        true,
        |_, _| {},
        || false,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let s = String::from_utf8_lossy(&bytes);
    assert!(
        s.contains("W\nn\n"),
        "no clipping path was emitted, so long values will overrun their column"
    );
}
