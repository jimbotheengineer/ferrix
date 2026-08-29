//! Tests for the PDF writer.
//!
//! Every structural assertion goes through [`super::reader::parse`], which
//! walks `startxref` → xref → catalog → page tree exactly as a viewer does.
//! Asserting on raw bytes instead would pass on a file with wrong offsets
//! that no reader can open — the single most likely way a hand-rolled writer
//! is broken.

use super::reader::{self, ParsedPdf};
use super::*;

const A4: (f32, f32) = (595.28, 841.89);

fn render(pages: usize, draw: impl Fn(usize, &mut Content)) -> Vec<u8> {
    let mut out = Vec::new();
    let mut doc = PdfDoc::new(&mut out, A4).unwrap();
    let mut c = Content::new(A4.1);
    for i in 0..pages {
        c.reset(A4.1);
        draw(i, &mut c);
        doc.add_page(&c).unwrap();
    }
    doc.finish().unwrap();
    out
}

fn parsed(pages: usize, draw: impl Fn(usize, &mut Content)) -> ParsedPdf {
    let bytes = render(pages, draw);
    reader::parse(&bytes).unwrap_or_else(|e| panic!("produced an unparseable PDF: {e}"))
}

#[test]
fn an_empty_document_still_parses() {
    let bytes = render(0, |_, _| {});
    let doc = reader::parse(&bytes).expect("zero-page document must still be a valid PDF");
    assert_eq!(doc.pages.len(), 0);
}

#[test]
fn text_written_is_text_read_back() {
    let doc = parsed(1, |_, c| {
        c.text(72.0, 100.0, 11.0, FONT_REGULAR, Color::BLACK, "Revenue");
        c.text(200.0, 100.0, 11.0, FONT_BOLD, Color::BLACK, "1,234.50");
    });
    assert_eq!(doc.page(1).strings(), vec!["Revenue", "1,234.50"]);
}

#[test]
fn xref_offsets_point_at_their_objects() {
    // `parse` verifies every object it visits begins with "<id> 0 obj" at the
    // offset the xref claims, so a successful parse of a multi-page document
    // is the assertion. Made explicit here because it is THE invariant the
    // rest of the suite leans on.
    let doc = parsed(5, |i, c| {
        c.text(
            72.0,
            100.0,
            10.0,
            FONT_REGULAR,
            Color::BLACK,
            &format!("page {}", i + 1),
        );
    });
    assert_eq!(doc.pages.len(), 5);
    for (i, p) in doc.pages.iter().enumerate() {
        assert_eq!(
            p.strings(),
            vec![format!("page {}", i + 1)],
            "page {} carried the wrong content stream — object ids or xref offsets are crossed",
            i + 1
        );
    }
}

#[test]
fn declared_page_count_matches_the_kids_array() {
    // parse() rejects a mismatch, so this pins that /Count is derived from
    // the same list the pages come from rather than being written blind.
    for n in [1usize, 2, 17] {
        let doc = parsed(n, |_, c| {
            c.text(10.0, 10.0, 8.0, FONT_REGULAR, Color::BLACK, "x");
        });
        assert_eq!(doc.pages.len(), n);
    }
}

#[test]
fn media_box_carries_the_requested_paper() {
    let doc = parsed(1, |_, c| {
        c.text(10.0, 10.0, 8.0, FONT_REGULAR, Color::BLACK, "x");
    });
    let (w, h) = doc.page(1).media;
    assert!((w - A4.0).abs() < 0.01, "media width was {w}");
    assert!((h - A4.1).abs() < 0.01, "media height was {h}");
}

#[test]
fn coordinates_are_flipped_from_top_down_to_pdf_space() {
    // A caller drawing 100pt from the TOP must land at (page height - 100)
    // in PDF space. Getting this wrong prints every page upside down while
    // every text-content assertion still passes.
    let doc = parsed(1, |_, c| {
        c.text(50.0, 100.0, 10.0, FONT_REGULAR, Color::BLACK, "near-top");
        c.text(50.0, 700.0, 10.0, FONT_REGULAR, Color::BLACK, "near-bottom");
    });
    let runs = &doc.page(1).texts;
    assert_eq!(runs.len(), 2);
    assert!(
        (runs[0].y - (A4.1 - 100.0)).abs() < 0.01,
        "top-down y=100 became PDF y={}",
        runs[0].y
    );
    assert!(
        runs[0].y > runs[1].y,
        "the row drawn nearer the top must have the LARGER PDF y ({} vs {})",
        runs[0].y,
        runs[1].y
    );
}

#[test]
fn parentheses_and_backslashes_survive_the_string_escape() {
    // An unescaped ')' terminates the PDF string early: the rest of the cell
    // silently vanishes and the operators after it are misparsed.
    let awkward = r"Net (adj) \ 50%";
    let doc = parsed(1, |_, c| {
        c.text(10.0, 20.0, 9.0, FONT_REGULAR, Color::BLACK, awkward);
    });
    assert_eq!(doc.page(1).strings(), vec![awkward]);
}

#[test]
fn a_stream_length_that_lies_is_caught() {
    // Corrupt /Length in an otherwise fine document; the reader must reject
    // it. This is what proves the length check in stream_at is load-bearing
    // rather than decorative.
    let mut bytes = render(1, |_, c| {
        c.text(10.0, 20.0, 9.0, FONT_REGULAR, Color::BLACK, "hello");
    });
    let pos = bytes
        .windows(11)
        .position(|w| w == b"<< /Length ")
        .expect("no length dict found");
    // Overwrite the first digit with a different one.
    let digit = pos + 11;
    bytes[digit] = if bytes[digit] == b'9' { b'1' } else { b'9' };
    let err = reader::parse(&bytes).unwrap_err();
    assert!(
        err.contains("Length") || err.contains("endstream"),
        "a corrupt /Length was not detected; error was {err:?}"
    );
}

#[test]
fn a_corrupt_xref_offset_is_caught() {
    let mut bytes = render(2, |_, c| {
        c.text(10.0, 20.0, 9.0, FONT_REGULAR, Color::BLACK, "hello");
    });
    // Locate the free entry that always opens the table, then clobber the
    // offset of the entry immediately after it. Anchoring on the free entry
    // rather than counting header bytes keeps this correct if the subsection
    // header's width ever changes.
    let free = bytes
        .windows(20)
        .rposition(|w| w == b"0000000000 65535 f \n")
        .expect("no free xref entry");
    let entry = free + 20;
    for b in bytes[entry..entry + 10].iter_mut() {
        *b = b'9';
    }
    assert!(
        reader::parse(&bytes).is_err(),
        "a bogus xref offset parsed as if it were fine"
    );
}

#[test]
fn fills_are_counted_and_clipping_nests() {
    let doc = parsed(1, |_, c| {
        c.fill_rect(10.0, 10.0, 100.0, 20.0, Color(255, 0, 0));
        c.fill_rect(10.0, 30.0, 100.0, 20.0, Color(0, 255, 0));
        c.clipped(10.0, 10.0, 100.0, 20.0, |c| {
            c.text(12.0, 24.0, 9.0, FONT_REGULAR, Color::BLACK, "inside");
        });
    });
    assert_eq!(doc.page(1).fills, 2);
    assert!(doc.page(1).contains("inside"));
}

#[test]
fn a_zero_area_rect_draws_nothing() {
    // A hidden row has height 0; emitting a degenerate `re` for it wastes
    // bytes on every page and some viewers render it as a hairline.
    let doc = parsed(1, |_, c| {
        c.fill_rect(10.0, 10.0, 100.0, 0.0, Color(255, 0, 0));
        c.fill_rect(10.0, 10.0, 0.0, 20.0, Color(255, 0, 0));
    });
    assert_eq!(doc.page(1).fills, 0);
}

#[test]
fn helvetica_widths_are_the_real_metrics() {
    // Right-aligning a column of numbers depends on these. A wrong table
    // still "works" — it just misaligns everything by a few points, which is
    // invisible in a unit test that only checks text content.
    //
    // Values from the Adobe Helvetica AFM: digits are all 556, 'i' is 222,
    // 'W' is 944, space is 278.
    let at12 = |s: &str| text_width(s, 12.0, false);
    assert!(
        (at12("0") - 6.672).abs() < 0.01,
        "digit width {}",
        at12("0")
    );
    assert!((at12("i") - 2.664).abs() < 0.01, "i width {}", at12("i"));
    assert!((at12("W") - 11.328).abs() < 0.01, "W width {}", at12("W"));
    // Digits are tabular: every digit must measure identically or columns of
    // numbers cannot line up.
    let widths: Vec<f32> = "0123456789".chars().map(|c| at12(&c.to_string())).collect();
    assert!(
        widths.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6),
        "digit advances differ: {widths:?}"
    );
}

#[test]
fn width_scales_with_point_size_and_bold_is_wider() {
    let a = text_width("Revenue", 10.0, false);
    let b = text_width("Revenue", 20.0, false);
    assert!(
        (b - a * 2.0).abs() < 0.01,
        "doubling the size did not double the width: {a} -> {b}"
    );
    assert!(
        text_width("Revenue", 10.0, true) > a,
        "bold measured no wider than regular"
    );
}

#[test]
fn non_winansi_characters_become_a_visible_placeholder() {
    // Dropping them would silently shorten a cell's text.
    let doc = parsed(1, |_, c| {
        c.text(10.0, 20.0, 9.0, FONT_REGULAR, Color::BLACK, "a\u{4e2d}b");
    });
    assert_eq!(doc.page(1).strings(), vec!["a?b"]);
}

#[test]
fn latin1_accents_are_preserved() {
    let doc = parsed(1, |_, c| {
        c.text(10.0, 20.0, 9.0, FONT_REGULAR, Color::BLACK, "Café");
    });
    assert_eq!(doc.page(1).strings(), vec!["Café"]);
}

#[test]
fn content_reuse_does_not_leak_between_pages() {
    // The whole streaming design rests on one reused buffer. If reset() ever
    // stopped clearing, page N would contain pages 1..N — which balloons a
    // large export quadratically and is easy to miss with one-page tests.
    let doc = parsed(3, |i, c| {
        c.text(
            10.0,
            20.0,
            9.0,
            FONT_REGULAR,
            Color::BLACK,
            &format!("only-{i}"),
        );
    });
    for (i, p) in doc.pages.iter().enumerate() {
        assert_eq!(
            p.texts.len(),
            1,
            "page {} has {} runs, so a previous page's content leaked in",
            i + 1,
            p.texts.len()
        );
    }
}

#[test]
fn a_thousand_pages_stay_bounded_in_memory() {
    // The scale claim, made checkable: the writer must not accumulate page
    // content. Peak is measured indirectly — the reused Content buffer never
    // grows past one page's worth, and the doc's own retained state is the
    // xref plus kids list.
    let mut sink = std::io::sink();
    let mut doc = PdfDoc::new(&mut sink, A4).unwrap();
    let mut c = Content::new(A4.1);
    let mut peak_content = 0usize;
    for i in 0..1000 {
        c.reset(A4.1);
        for r in 0..40 {
            c.text(
                40.0,
                40.0 + r as f32 * 15.0,
                9.0,
                FONT_REGULAR,
                Color::BLACK,
                &format!("r{r} p{i}"),
            );
        }
        peak_content = peak_content.max(c.as_bytes().len());
        doc.add_page(&c).unwrap();
    }
    assert_eq!(doc.page_count(), 1000);
    assert!(
        peak_content < 64 * 1024,
        "one page's content stream grew to {peak_content} bytes, so state is accumulating"
    );
    doc.finish().unwrap();
}
