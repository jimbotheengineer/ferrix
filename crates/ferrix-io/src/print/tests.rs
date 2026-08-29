//! Tests for the streaming print export.
//!
//! Verification principle (issue #37): a PDF is checked by **extracting its
//! text from the bytes on disk and asserting cell values land on the expected
//! page** — never by eyeballing a render. The extractor below parses the real
//! PDF structure (the `/Kids` page order and each page's `/Contents` stream),
//! so every assertion runs through the serialized format, not the in-memory
//! data the writer held.

use super::*;
use ferrix_core::page::{PageSetup, Paginator};
use ferrix_core::{CellRef, ColSizes, RowSizes};

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join("ferrix_print_tests");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A grid sheet: value at (r,c) is the string "r{r}c{c}", so a cell's text
/// encodes its own coordinates and can be matched unambiguously in the output.
struct Grid {
    rows: usize,
    cols: usize,
}

impl ExportSource for Grid {
    fn row_count(&self) -> usize {
        self.rows
    }
    fn col_count(&self) -> usize {
        self.cols
    }
    fn display(&self, cell: CellRef) -> String {
        format!("r{}c{}", cell.row, cell.col)
    }
    fn header(&self, col: usize) -> String {
        format!("H{col}")
    }
}

/// Build a paginator whose pages hold `rows_per_page` rows and
/// `cols_per_page` columns, by choosing paper/margins/row-heights that force
/// exactly that split. We do it with manual breaks for determinism.
fn paginator_with_breaks(
    rows: u32,
    cols: u32,
    row_breaks: &[u32],
    col_breaks: &[u32],
) -> Paginator {
    let mut setup = PageSetup::default();
    for &b in row_breaks {
        setup.add_row_break(b);
    }
    for &b in col_breaks {
        setup.add_col_break(b);
    }
    Paginator::new(
        setup,
        (0, rows - 1),
        (0, cols - 1),
        &RowSizes::default(),
        &ColSizes::default(),
    )
}

// ------------------------------------------------------------------
// A minimal PDF reader, for tests only. Parses the serialized file.
// ------------------------------------------------------------------

/// Extract, per page (in /Kids order), the list of text literals drawn on it.
fn pdf_page_texts(bytes: &[u8]) -> Vec<Vec<String>> {
    let s = bytes;
    // Map object number -> byte range of its body (between "N 0 obj" and "endobj").
    let text = String::from_utf8_lossy(s);
    let mut obj_body: std::collections::HashMap<u64, (usize, usize)> =
        std::collections::HashMap::new();
    let mut i = 0;
    while let Some(rel) = text[i..].find(" 0 obj") {
        let at = i + rel;
        // Walk back to the object number.
        let start_num = text[..at]
            .rfind(|c: char| !c.is_ascii_digit())
            .map(|p| p + 1)
            .unwrap_or(0);
        if let Ok(num) = text[start_num..at].parse::<u64>() {
            if let Some(erel) = text[at..].find("endobj") {
                obj_body.insert(num, (at + 6, at + erel));
            }
        }
        i = at + 6;
    }

    // Find the Pages object's /Kids [ ... ] order.
    let pages_body = obj_body
        .values()
        .map(|&(a, b)| &text[a..b])
        .find(|body| body.contains("/Type /Pages"))
        .expect("a /Pages object");
    let kids_start = pages_body.find("/Kids").expect("Kids");
    let lb = pages_body[kids_start..].find('[').unwrap() + kids_start;
    let rb = pages_body[lb..].find(']').unwrap() + lb;
    let kids: Vec<u64> = pages_body[lb + 1..rb]
        .split("0 R")
        .filter_map(|t| t.split_whitespace().next())
        .filter_map(|t| t.parse::<u64>().ok())
        .collect();

    let mut out = Vec::new();
    for pid in kids {
        let (a, b) = obj_body[&pid];
        let body = &text[a..b];
        // /Contents N 0 R
        let cpos = body.find("/Contents").expect("Contents");
        let cnum: u64 = body[cpos + 9..]
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        // The content stream is between "stream\n" and "\nendstream" of that obj.
        let (ca, cb) = obj_body[&cnum];
        let cbody = &s[ca..cb];
        let cstr = String::from_utf8_lossy(cbody);
        let sstart = cstr.find("stream").unwrap() + "stream".len();
        // skip the newline after "stream"
        let sstart = sstart
            + if cstr[sstart..].starts_with("\r\n") {
                2
            } else {
                1
            };
        let send = cstr.find("endstream").unwrap();
        let stream = &cstr[sstart..send];
        out.push(extract_literals(stream));
    }
    out
}

/// Pull the `(...)` string literals that precede a `Tj`, honouring `\(`, `\)`,
/// `\\` escapes.
fn extract_literals(stream: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = stream.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            let mut j = i + 1;
            let mut lit = String::new();
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' if j + 1 < bytes.len() => {
                        lit.push(bytes[j + 1] as char);
                        j += 2;
                    }
                    b')' => {
                        j += 1;
                        break;
                    }
                    c => {
                        lit.push(c as char);
                        j += 1;
                    }
                }
            }
            out.push(lit);
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn noop_progress(_: u64, _: u64) {}
fn never_cancel() -> bool {
    false
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[test]
fn a_cell_lands_on_the_page_the_paginator_assigned_it() {
    // 4 rows x 4 cols, split into 2x2 pages by manual breaks before row 2 and
    // col 2. That is four pages of a 2x2 block each.
    let p = paginator_with_breaks(4, 4, &[2], &[2]);
    assert_eq!(p.page_count(), 4);

    let out = scratch().join("grid.pdf");
    let src = Grid { rows: 4, cols: 4 };
    export_pdf(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap();

    let bytes = read(&out);
    assert!(bytes.starts_with(b"%PDF-1.7"), "must be a PDF");
    let per_page = pdf_page_texts(&bytes);
    assert_eq!(per_page.len(), 4, "four pages of text");

    // Build page-number -> which cell coords appear.
    // Default page order is DownThenOver: page index = ci * row_bands + ri.
    // row_bands = [(0,1),(2,3)], col_bands = [(0,1),(2,3)].
    // The exact page a cell lands on must match Paginator::page_at.
    for r in 0..4u32 {
        for c in 0..4u32 {
            let page = p.page_at(r, c).unwrap();
            let want = format!("r{r}c{c}");
            let idx = (page.number - 1) as usize;
            assert!(
                per_page[idx].iter().any(|t| t == &want),
                "cell {want} must appear on page {} (its paginator page); \
                 that page's text was {:?}",
                page.number,
                per_page[idx]
            );
            // And it must NOT appear on any other page.
            for (other, texts) in per_page.iter().enumerate() {
                if other != idx {
                    assert!(
                        !texts.iter().any(|t| t == &want),
                        "cell {want} leaked onto page {} as well",
                        other + 1
                    );
                }
            }
        }
    }
}

#[test]
fn what_would_this_report_if_the_export_did_nothing() {
    // Negative control: a sheet with a known value must NOT produce a PDF whose
    // text is empty. If export_pdf were a stub that wrote a header and no pages,
    // pdf_page_texts would be empty and this fails.
    let p = paginator_with_breaks(2, 2, &[], &[]);
    let out = scratch().join("nonempty.pdf");
    let src = Grid { rows: 2, cols: 2 };
    export_pdf(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap();
    let per_page = pdf_page_texts(&read(&out));
    let total: usize = per_page.iter().map(|v| v.len()).sum();
    assert!(
        total >= 4,
        "every one of the 4 cells must be drawn, got {total} literals"
    );
}

#[test]
fn header_footer_field_codes_are_resolved_in_the_pdf() {
    let mut setup = PageSetup::default();
    setup.header.left = "&F".into();
    setup.footer.center = "Page &P of &N".into();
    let p = Paginator::new(
        setup,
        (0, 1),
        (0, 1),
        &RowSizes::default(),
        &ColSizes::default(),
    );

    let out = scratch().join("hf.pdf");
    let src = Grid { rows: 2, cols: 2 };
    let ctx = PrintContext {
        file: "budget.fx".into(),
        sheet: "Q1".into(),
        date: "2026-01-01".into(),
        time: "09:00".into(),
    };
    export_pdf(
        &out,
        &src,
        &p,
        &ctx,
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap();

    let per_page = pdf_page_texts(&read(&out));
    let all: Vec<String> = per_page.into_iter().flatten().collect();
    assert!(
        all.iter().any(|t| t == "budget.fx"),
        "&F must resolve to the file name: {all:?}"
    );
    assert!(
        all.iter().any(|t| t == "Page 1 of 1"),
        "&P/&N must resolve to page numbers: {all:?}"
    );
}

#[test]
fn a_large_job_is_refused_until_forced() {
    // Force many pages: one row per page via manual breaks before every row.
    let breaks: Vec<u32> = (1..1200).collect();
    let p = paginator_with_breaks(1200, 1, &breaks, &[]);
    assert!(p.is_large(), "1200 pages should be large");

    let out = scratch().join("huge.pdf");
    let _ = std::fs::remove_file(&out); // clear any stale file from a prior run
    let src = Grid {
        rows: 1200,
        cols: 1,
    };
    let err = export_pdf(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap_err();
    match err {
        PrintError::TooLarge(l) => assert_eq!(l.pages, p.page_count()),
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert!(!out.exists(), "a refused job must leave no file behind");

    // Forcing it through produces the file.
    export_pdf(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions { force: true },
        noop_progress,
        never_cancel,
    )
    .unwrap();
    assert!(out.exists());
}

#[test]
fn cancelling_leaves_no_output_file() {
    let p = paginator_with_breaks(10, 2, &(1..10).collect::<Vec<_>>(), &[]);
    let out = scratch().join("cancel.pdf");
    let _ = std::fs::remove_file(&out); // clear any stale file from a prior run
    let src = Grid { rows: 10, cols: 2 };
    let err = export_pdf(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        || true, // cancel immediately
    )
    .unwrap_err();
    assert!(matches!(err, PrintError::Cancelled));
    assert!(!out.exists(), "cancel must not leave a partial file");
}

#[test]
fn repeat_rows_appear_on_every_page() {
    // Repeat row 0 at the top of each page; break rows into two pages.
    let mut setup = PageSetup {
        repeat_rows: Some((0, 0)),
        ..Default::default()
    };
    setup.add_row_break(3);
    let p = Paginator::new(
        setup,
        (0, 5),
        (0, 1),
        &RowSizes::default(),
        &ColSizes::default(),
    );
    assert!(p.page_count() >= 2);

    let out = scratch().join("repeat.pdf");
    let src = Grid { rows: 6, cols: 2 };
    export_pdf(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap();

    let per_page = pdf_page_texts(&read(&out));
    // Row 0 cell "r0c0" must appear on every page.
    for (i, texts) in per_page.iter().enumerate() {
        assert!(
            texts.iter().any(|t| t == "r0c0"),
            "repeated row 0 (r0c0) must appear on page {}: {:?}",
            i + 1,
            texts
        );
    }
}

#[test]
fn html_export_contains_the_cells_and_is_one_file() {
    let p = paginator_with_breaks(3, 3, &[], &[]);
    let out = scratch().join("grid.html");
    let src = Grid { rows: 3, cols: 3 };
    export_html(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap();
    let html = String::from_utf8(read(&out)).unwrap();
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("<table"));
    for r in 0..3u32 {
        for c in 0..3u32 {
            assert!(
                html.contains(&format!("r{r}c{c}")),
                "cell r{r}c{c} must be in the HTML"
            );
        }
    }
    // Single file: no external references.
    assert!(!html.contains("<link"), "must be self-contained");
    assert!(!html.contains("src="), "must be self-contained");
}

#[test]
fn html_escapes_dangerous_cell_text() {
    struct Evil;
    impl ExportSource for Evil {
        fn row_count(&self) -> usize {
            1
        }
        fn col_count(&self) -> usize {
            1
        }
        fn display(&self, _: CellRef) -> String {
            "<script>&\"x".into()
        }
        fn header(&self, _: usize) -> String {
            "h".into()
        }
    }
    let p = paginator_with_breaks(1, 1, &[], &[]);
    let out = scratch().join("evil.html");
    export_html(
        &out,
        &Evil,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap();
    let html = String::from_utf8(read(&out)).unwrap();
    assert!(!html.contains("<script>"), "raw script tag must be escaped");
    assert!(
        html.contains("&lt;script&gt;"),
        "must contain the escaped form"
    );
}

#[test]
fn pdf_escapes_parentheses_so_the_stream_stays_valid() {
    struct Parens;
    impl ExportSource for Parens {
        fn row_count(&self) -> usize {
            1
        }
        fn col_count(&self) -> usize {
            1
        }
        fn display(&self, _: CellRef) -> String {
            "a(b)c\\d".into()
        }
        fn header(&self, _: usize) -> String {
            "h".into()
        }
    }
    let p = paginator_with_breaks(1, 1, &[], &[]);
    let out = scratch().join("parens.pdf");
    export_pdf(
        &out,
        &Parens,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap();
    let per_page = pdf_page_texts(&read(&out));
    let all: Vec<String> = per_page.into_iter().flatten().collect();
    // The extractor decodes the escapes, so it recovers the original string.
    assert!(
        all.iter().any(|t| t == "a(b)c\\d"),
        "parentheses/backslash must round-trip through PDF escaping: {all:?}"
    );
}

#[test]
fn every_pdf_object_offset_in_the_xref_is_correct() {
    // A structural check: the xref offsets must point at the start of the
    // object they name. A reader relies on this; a wrong offset is a silently
    // broken PDF that some viewers "recover" and others reject.
    let p = paginator_with_breaks(3, 3, &[2], &[]);
    let out = scratch().join("xref.pdf");
    let src = Grid { rows: 3, cols: 3 };
    export_pdf(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions::default(),
        noop_progress,
        never_cancel,
    )
    .unwrap();
    let bytes = read(&out);
    let text = String::from_utf8_lossy(&bytes);

    // Parse the xref table. Anchor on "\nxref\n": the trailer's "startxref"
    // also ends in "xref" but is preceded by 't', not a newline, so this finds
    // the table and not the pointer to it.
    let xref_at = text.rfind("\nxref\n").expect("xref table");
    let after = &text[xref_at + 6..];
    let mut lines = after.lines();
    let count: usize = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    // First entry is the free object 0; entries 1..count are our objects.
    let mut entries = Vec::new();
    for _ in 0..count {
        entries.push(lines.next().unwrap().to_string());
    }
    for (obj, line) in entries.iter().enumerate().skip(1) {
        if line.trim_end().ends_with('f') {
            continue;
        }
        let off: usize = line[..10].parse().unwrap();
        let expect = format!("{obj} 0 obj");
        // Index the raw bytes: the xref offsets are byte offsets into the file,
        // and the binary header comment makes the lossy String misaligned.
        assert!(
            bytes[off..].starts_with(expect.as_bytes()),
            "xref says object {obj} is at offset {off}, but that is {:?}",
            String::from_utf8_lossy(&bytes[off..(off + expect.len()).min(bytes.len())])
        );
    }
}

#[test]
fn one_page_at_a_time_survives_a_thousand_pages_without_holding_the_document() {
    // Not a memory probe (the harness can't measure RSS here), but a proxy: a
    // 1000-page forced export completes and every page carries its own cell,
    // which a "collect all pages into a Vec<String> first" implementation would
    // also pass — so this is a liveness/scale smoke test, not a bound proof.
    // The bound is argued structurally in the module docs (one Page + one
    // content buffer at a time). Documented as NOT independently verified.
    let breaks: Vec<u32> = (1..1000).collect();
    let p = paginator_with_breaks(1000, 1, &breaks, &[]);
    let out = scratch().join("thousand.pdf");
    let src = Grid {
        rows: 1000,
        cols: 1,
    };
    let stats = export_pdf(
        &out,
        &src,
        &p,
        &PrintContext::default(),
        &PrintOptions { force: true },
        noop_progress,
        never_cancel,
    )
    .unwrap();
    assert_eq!(p.page_count(), 1000);
    assert!(stats.bytes > 0);
    // Spot-check the last page holds its row.
    let per_page = pdf_page_texts(&read(&out));
    assert_eq!(per_page.len(), 1000);
    assert!(per_page[999].iter().any(|t| t == "r999c0"));
}
