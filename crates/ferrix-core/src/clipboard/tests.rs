//! Tests for the rich clipboard flavour.
//!
//! Every assertion here is written against a value the feature produces — a
//! cell's text at a coordinate, a resolved number format, a rectangle count —
//! rather than against "something happened". The question asked of each is:
//! *what would this assert if `from_html` returned an empty block?*

use super::*;
use crate::table::DateStyle;

fn plain(rows: &[&[&str]]) -> ClipBlock {
    let grid: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect();
    ClipBlock::from_text_grid(&grid)
}

fn text_at(b: &ClipBlock, r: usize, c: usize) -> String {
    b.get(r, c).map(|x| x.text.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------- detection --

#[test]
fn html_is_detected_only_when_a_table_is_present() {
    assert!(looks_like_html("<table><tr><td>1</td></tr></table>"));
    assert!(looks_like_html(
        "<html><body>\n<TABLE>\n<TR><TD>1</TD></TR></TABLE>"
    ));
    // Data that merely contains angle brackets is DATA. Misreading it as
    // markup would eat the user's content.
    assert!(!looks_like_html("a<b\tc>d"));
    assert!(!looks_like_html("1\t2\r\n3\t4"));
    assert!(!looks_like_html("<div>not a table</div>"));
}

#[test]
fn html_sniffing_only_reads_the_head_of_a_huge_paste() {
    // A `<table` a megabyte into a text paste is not a clipboard flavour, and
    // scanning the whole payload to find out would make sniffing cost O(size).
    let mut s = "x\ty\r\n".repeat(200_000);
    s.push_str("<table><tr><td>1</td></tr></table>");
    assert!(!looks_like_html(&s));
}

// ------------------------------------------------------------ HTML -> block --

#[test]
fn an_excel_shaped_table_parses_to_the_right_values() {
    // The shape Excel actually writes: a <th> header row, inline styles, and
    // mso-number-format on the formatted cells.
    let html = r##"<html><body><table border=0>
<tr><th>Name</th><th>Amount</th></tr>
<tr><td>Widget</td><td style="mso-number-format:'\0022$\0022#,##0.00'" align=right>$1,234.50</td></tr>
<tr><td>Gadget</td><td style='mso-number-format:"#,##0.00"' align=right>99.00</td></tr>
</table></body></html>"##;
    let b = from_html(html).expect("an Excel table must parse");
    assert_eq!((b.rows(), b.cols()), (3, 2), "3 rows x 2 cols");
    assert_eq!(text_at(&b, 0, 0), "Name");
    assert_eq!(text_at(&b, 1, 0), "Widget");
    assert_eq!(text_at(&b, 1, 1), "$1,234.50");
    assert_eq!(text_at(&b, 2, 1), "99.00");
    // The header row's <th> carries boldness, which is why an Excel header
    // pastes in looking like one.
    assert_eq!(b.get(0, 0).unwrap().style.typography.bold, Some(true));
    assert_eq!(b.get(1, 0).unwrap().style.typography.bold, None);
}

#[test]
fn the_html_flavour_is_preferred_over_the_text_one() {
    // The acceptance criterion. The SAME payload read as TSV would be a single
    // 1x1 cell of markup; read as HTML it is the 2x2 grid Excel meant.
    let html = "<table><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table>";
    let b = parse_clipboard(html);
    assert_eq!((b.rows(), b.cols()), (2, 2));
    assert_eq!(text_at(&b, 1, 1), "4");
    assert!(
        !text_at(&b, 0, 0).contains('<'),
        "markup must not land in a cell as literal text"
    );
}

#[test]
fn plain_text_still_goes_down_the_tsv_path() {
    // The other half: HTML support must not break the TSV path that already
    // worked, including its quoting.
    let b = parse_clipboard("a\tb\r\n\"has\ttab\"\td");
    assert_eq!((b.rows(), b.cols()), (2, 2));
    assert_eq!(text_at(&b, 0, 1), "b");
    assert_eq!(text_at(&b, 1, 0), "has\ttab", "TSV quoting still honoured");
}

#[test]
fn entities_and_line_breaks_inside_cells_are_decoded() {
    let html =
        "<table><tr><td>a &amp; b</td><td>1 &lt; 2</td><td>x<br>y</td><td>&#65;&#x42;</td></tr></table>";
    let b = from_html(html).unwrap();
    assert_eq!(text_at(&b, 0, 0), "a & b");
    assert_eq!(text_at(&b, 0, 1), "1 < 2");
    assert_eq!(text_at(&b, 0, 2), "x\ny", "<br> is a newline, not nothing");
    assert_eq!(text_at(&b, 0, 3), "AB", "numeric entities decode");
}

#[test]
fn nested_markup_inside_a_cell_is_reduced_to_its_text() {
    // Excel and browsers wrap cell content in <font>, <span>, <p>. The value
    // is the text, and the markup must never reach the sheet.
    let html = "<table><tr><td><font color=\"#FF0000\"><b>42</b></font></td></tr></table>";
    let b = from_html(html).unwrap();
    assert_eq!(text_at(&b, 0, 0), "42");
}

#[test]
fn colspan_keeps_later_cells_under_the_right_columns() {
    // A merged header cell spans two columns. Without honouring colspan every
    // cell after it slides one column left — data under the wrong header,
    // silently and plausibly.
    let html = "<table>\
<tr><td colspan=\"2\">Q1</td><td>Q2</td></tr>\
<tr><td>10</td><td>20</td><td>30</td></tr></table>";
    let b = from_html(html).unwrap();
    assert_eq!(b.cols(), 3);
    assert_eq!(text_at(&b, 0, 0), "Q1");
    assert_eq!(text_at(&b, 0, 1), "", "the spanned column is blank");
    assert_eq!(text_at(&b, 0, 2), "Q2", "Q2 stays in column 3");
    assert_eq!(text_at(&b, 1, 2), "30");
}

#[test]
fn ragged_rows_are_padded_to_a_rectangle() {
    let html = "<table><tr><td>a</td><td>b</td><td>c</td></tr><tr><td>d</td></tr></table>";
    let b = from_html(html).unwrap();
    assert_eq!((b.rows(), b.cols()), (2, 3));
    assert_eq!(text_at(&b, 1, 0), "d");
    assert_eq!(text_at(&b, 1, 2), "");
}

#[test]
fn a_table_with_no_rows_is_none_not_an_empty_block() {
    // Returning an empty block would let a broken paste look like a
    // successful paste of nothing.
    assert!(from_html("<table></table>").is_none());
    assert!(from_html("no markup at all").is_none());
}

#[test]
fn a_tableless_html_payload_falls_back_to_text_rather_than_vanishing() {
    let b = parse_clipboard("<table-ish\tnot really");
    assert_eq!(text_at(&b, 0, 0), "<table-ish");
}

#[test]
fn column_widths_are_read_from_the_colgroup() {
    let html = "<table><colgroup><col width=\"120\"><col><col width=\"60pt\"></colgroup>\
<tr><td>a</td><td>b</td><td>c</td></tr></table>";
    let b = from_html(html).unwrap();
    assert_eq!(b.col_widths.len(), 3);
    // A bare number on <col width> is pixels; 120px is 90pt.
    assert_eq!(b.col_widths[0], Some(90.0));
    assert_eq!(b.col_widths[1], None, "an unsized column stays unsized");
    assert_eq!(b.col_widths[2], Some(60.0), "an explicit pt is taken as pt");
}

#[test]
fn styles_are_read_off_the_cells() {
    let html = "<table><tr>\
<td style=\"background-color:#FFFF00;color:#FF0000;font-weight:bold\">a</td>\
<td style=\"font-style:italic;text-decoration:underline line-through\">b</td>\
<td style=\"font-family:Consolas,monospace;font-size:14pt\">c</td>\
<td style=\"color:rgb(0, 128, 255)\">d</td>\
<td style=\"background-color:#0f0\">e</td>\
</tr></table>";
    let b = from_html(html).unwrap();
    let a = &b.get(0, 0).unwrap().style;
    assert_eq!(a.fill, Some(Rgb(0xFF, 0xFF, 0x00)));
    assert_eq!(a.text, Some(Rgb(0xFF, 0x00, 0x00)));
    assert_eq!(a.typography.bold, Some(true));

    let s = &b.get(0, 1).unwrap().style.typography;
    assert_eq!(s.italic, Some(true));
    assert_eq!(s.underline, Some(true));
    assert_eq!(
        s.strikethrough,
        Some(true),
        "both decorations must survive one text-decoration declaration"
    );

    let t = &b.get(0, 2).unwrap().style.typography;
    assert_eq!(t.family, Some(FontFamily::Monospace));
    assert_eq!(t.size, Some(14.0));

    assert_eq!(b.get(0, 3).unwrap().style.text, Some(Rgb(0, 128, 255)));
    assert_eq!(
        b.get(0, 4).unwrap().style.fill,
        Some(Rgb(0x00, 0xFF, 0x00)),
        "#abc shorthand expands"
    );
}

#[test]
fn a_pasted_font_size_is_clamped_to_something_renderable() {
    // A 400pt cell would draw over its neighbours; the toolbar already
    // clamps, and a paste is not a way around that.
    let html = "<table><tr><td style=\"font-size:400pt\">a</td></tr></table>";
    let b = from_html(html).unwrap();
    assert_eq!(
        b.get(0, 0).unwrap().style.typography.size,
        Some(crate::format::MAX_FONT_PT)
    );
}

#[test]
fn an_mso_number_format_with_a_semicolon_is_not_split_in_half() {
    // The classic parser bug: a two-section code contains a `;`, which is also
    // the CSS declaration separator. Splitting naively truncates the format.
    let html = "<table><tr><td style=\"mso-number-format:'#,##0.00;[Red]\\(#,##0.00\\)';color:#0000FF\">1.00</td></tr></table>";
    let b = from_html(html).unwrap();
    let cell = b.get(0, 0).unwrap();
    let code = cell
        .format
        .as_ref()
        .map(|f| f.to_code())
        .unwrap_or_default();
    assert!(
        code.contains(';') && code.contains("[Red]"),
        "the whole two-section code must survive, got {code:?}"
    );
    assert_eq!(
        cell.style.text,
        Some(Rgb(0, 0, 0xFF)),
        "and the declaration after it must still be read"
    );
}

#[test]
fn an_excel_style_formula_attribute_is_read() {
    let html = "<table><tr><td x:fmla=\"=SUM(A1:A3)\">6</td></tr></table>";
    let b = from_html(html).unwrap();
    assert_eq!(b.get(0, 0).unwrap().formula.as_deref(), Some("=SUM(A1:A3)"));
}

// ------------------------------------------------------------ block -> HTML --

#[test]
fn rendered_html_is_a_table_with_one_td_per_cell() {
    let b = plain(&[&["a", "b"], &["c", "d"]]);
    let html = to_html(&b);
    assert_eq!(html.matches("<tr>").count(), 2, "one <tr> per row");
    assert_eq!(html.matches("<td").count(), 4, "one <td> per cell");
    assert!(html.contains(">a</td>"));
    assert!(html.contains(">d</td>"));
}

#[test]
fn rendered_html_escapes_content_that_would_otherwise_be_markup() {
    let mut b = ClipBlock::new(1, 1);
    b.set(0, 0, ClipCell::text("<b>&\"x\""));
    let html = to_html(&b);
    assert!(
        html.contains("&lt;b&gt;&amp;"),
        "unescaped content would change the table's structure: {html}"
    );
    // And it must come back exactly.
    assert_eq!(text_at(&from_html(&html).unwrap(), 0, 0), "<b>&\"x\"");
}

#[test]
fn a_number_format_is_written_where_excel_looks_for_it() {
    let mut b = ClipBlock::new(1, 1);
    b.set(
        0,
        0,
        ClipCell {
            text: "$1,234.50".into(),
            format: Some(NumberFormat::Currency {
                symbol: "$".into(),
                places: 2,
            }),
            ..Default::default()
        },
    );
    let html = to_html(&b);
    assert!(
        html.contains("mso-number-format:"),
        "Excel reads the format from mso-number-format: {html}"
    );
}

// -------------------------------------------------------------- round trips --

#[test]
fn a_round_trip_preserves_values_formats_and_styling() {
    // THE acceptance criterion: Ferrix -> clipboard -> Ferrix keeps values,
    // number formats and styling. Asserted field by field, so a round trip
    // that dropped only the styling still fails.
    let mut b = ClipBlock::new(2, 2);
    b.set(
        0,
        0,
        ClipCell {
            text: "1,234.50".into(),
            formula: None,
            origin: None,
            format: Some(NumberFormat::Thousands { places: 2 }),
            style: ManualStyle {
                fill: Some(Rgb(0xFF, 0xEE, 0x00)),
                text: Some(Rgb(0x11, 0x22, 0x33)),
                typography: Typography::default()
                    .with_bold(true)
                    .with_italic(true)
                    .with_size(13.0)
                    .with_family(FontFamily::Monospace),
            },
        },
    );
    b.set(
        0,
        1,
        ClipCell {
            text: "42%".into(),
            format: Some(NumberFormat::Percent { places: 0 }),
            ..Default::default()
        },
    );
    b.set(
        1,
        0,
        ClipCell {
            text: "2024-01-31".into(),
            format: Some(NumberFormat::Date(DateStyle::Iso)),
            ..Default::default()
        },
    );
    b.set(1, 1, ClipCell::text("plain"));
    b.col_widths = vec![Some(90.0), Some(150.0)];

    let back = from_html(&to_html(&b)).expect("round trip must parse");

    assert_eq!((back.rows(), back.cols()), (2, 2));
    for (r, c) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
        let a = b.get(r, c).unwrap();
        let z = back.get(r, c).unwrap();
        assert_eq!(z.text, a.text, "value at ({r},{c})");
        assert_eq!(z.format, a.format, "number format at ({r},{c})");
        assert_eq!(z.style.fill, a.style.fill, "fill at ({r},{c})");
        assert_eq!(z.style.text, a.style.text, "text colour at ({r},{c})");
        assert_eq!(
            z.style.typography.bold, a.style.typography.bold,
            "bold at ({r},{c})"
        );
        assert_eq!(
            z.style.typography.italic, a.style.typography.italic,
            "italic at ({r},{c})"
        );
        assert_eq!(
            z.style.typography.size, a.style.typography.size,
            "font size at ({r},{c})"
        );
        assert_eq!(
            z.style.typography.family, a.style.typography.family,
            "font family at ({r},{c})"
        );
    }
    assert_eq!(back.col_widths, b.col_widths, "column widths survive");
}

#[test]
fn a_formula_round_trips_as_text_with_its_dollars_intact() {
    // Formulas are carried as SOURCE TEXT. A round trip through the parser
    // would drop `$`, and the damage would only surface on the next fill.
    let mut b = ClipBlock::new(1, 1);
    b.set(
        0,
        0,
        ClipCell {
            text: "60".into(),
            formula: Some("=SUM($A$1:A10)*LOG10(B2)".into()),
            ..Default::default()
        },
    );
    let back = from_html(&to_html(&b)).unwrap();
    assert_eq!(
        back.get(0, 0).unwrap().formula.as_deref(),
        Some("=SUM($A$1:A10)*LOG10(B2)"),
        "every $ must survive verbatim"
    );
    assert_eq!(back.get(0, 0).unwrap().text, "60", "and the cached value");
}

#[test]
fn a_formula_containing_quotes_and_angle_brackets_round_trips() {
    let mut b = ClipBlock::new(1, 1);
    b.set(
        0,
        0,
        ClipCell {
            text: "yes".into(),
            formula: Some("=IF(A1<B1,\"a<b\",\"a>=b\")".into()),
            ..Default::default()
        },
    );
    let back = from_html(&to_html(&b)).unwrap();
    assert_eq!(
        back.get(0, 0).unwrap().formula.as_deref(),
        Some("=IF(A1<B1,\"a<b\",\"a>=b\")")
    );
}

#[test]
fn a_round_trip_through_tsv_alone_loses_formatting() {
    // The reason the HTML flavour exists at all. If this ever passes, the two
    // paths have become the same and one of them is dead.
    let mut b = ClipBlock::new(1, 1);
    b.set(
        0,
        0,
        ClipCell {
            text: "1234.5".into(),
            format: Some(NumberFormat::Thousands { places: 2 }),
            ..Default::default()
        },
    );
    let via_tsv = ClipBlock::from_text_grid(&crate::tsv::from_tsv(&crate::tsv::to_tsv(
        &b.to_text_grid(),
    )));
    assert_eq!(via_tsv.get(0, 0).unwrap().text, "1234.5", "values survive");
    assert_eq!(
        via_tsv.get(0, 0).unwrap().format,
        None,
        "TSV cannot carry a number format — that is why to_html exists"
    );
}

// --------------------------------------------------------------- transpose --

#[test]
fn transpose_swaps_axes_and_carries_everything_with_them() {
    let mut b = ClipBlock::new(2, 3);
    for r in 0..2 {
        for c in 0..3 {
            b.set(
                r,
                c,
                ClipCell {
                    text: format!("{r}{c}"),
                    format: Some(NumberFormat::Decimal { places: r as u8 }),
                    ..Default::default()
                },
            );
        }
    }
    let t = b.transposed();
    assert_eq!((t.rows(), t.cols()), (3, 2));
    assert_eq!(text_at(&t, 2, 1), "12", "(1,2) must land at (2,1)");
    assert_eq!(
        t.get(2, 1).unwrap().format,
        Some(NumberFormat::Decimal { places: 1 }),
        "the format travels with its value"
    );
    assert_eq!(b.transposed().transposed(), b, "transpose is an involution");
}

#[test]
fn transpose_drops_column_widths_rather_than_applying_them_to_the_wrong_axis() {
    let mut b = ClipBlock::new(1, 2);
    b.col_widths = vec![Some(100.0), Some(50.0)];
    let t = b.transposed();
    // One column after the transpose, and it is unsized: the source's widths
    // describe what are now ROWS, so applying them would resize the wrong axis.
    assert_eq!(t.cols(), 1);
    assert_eq!(t.col_widths, vec![None], "widths describe rows now");
}

// ------------------------------------------------------------- paste maths --

#[test]
fn arithmetic_operations_combine_destination_with_source() {
    assert_eq!(PasteOp::Add.apply(Some(10.0), Some(3.0)), Some(13.0));
    assert_eq!(PasteOp::Subtract.apply(Some(10.0), Some(3.0)), Some(7.0));
    assert_eq!(PasteOp::Multiply.apply(Some(10.0), Some(3.0)), Some(30.0));
    assert_eq!(PasteOp::Divide.apply(Some(12.0), Some(4.0)), Some(3.0));
    assert_eq!(PasteOp::None.apply(Some(10.0), Some(3.0)), Some(3.0));
}

#[test]
fn arithmetic_ordering_is_destination_op_source_not_the_reverse() {
    // Subtract and Divide are not commutative, and getting the order backwards
    // is the bug a symmetric test (10 + 3) could never see.
    assert_eq!(
        PasteOp::Subtract.apply(Some(10.0), Some(3.0)),
        Some(7.0),
        "must be dest - src"
    );
    assert_eq!(
        PasteOp::Divide.apply(Some(10.0), Some(4.0)),
        Some(2.5),
        "must be dest / src"
    );
}

#[test]
fn an_empty_destination_counts_as_zero_for_arithmetic() {
    assert_eq!(PasteOp::Add.apply(None, Some(5.0)), Some(5.0));
    assert_eq!(PasteOp::Subtract.apply(None, Some(5.0)), Some(-5.0));
}

#[test]
fn arithmetic_refuses_rather_than_writing_nonsense() {
    // Non-numeric source: nothing sensible to add. Refusing leaves the
    // destination alone; writing #VALUE! over data the user did not ask to
    // touch is worse.
    assert_eq!(PasteOp::Add.apply(Some(1.0), None), None);
    assert_eq!(
        PasteOp::Divide.apply(Some(1.0), Some(0.0)),
        None,
        "divide by zero must not produce an infinity"
    );
}

#[test]
fn formatted_numbers_are_still_numbers_for_arithmetic() {
    // The case a user actually reaches for Paste Special > Add with: a column
    // of currency. If grouping separators defeated the parse, every formatted
    // cell would be silently skipped.
    assert_eq!(ClipCell::text("1,234.50").as_number(), Some(1234.50));
    assert_eq!(ClipCell::text("$1,234.50").as_number(), Some(1234.50));
    assert_eq!(ClipCell::text("(500)").as_number(), Some(-500.0));
    assert_eq!(ClipCell::text("42%").as_number(), Some(0.42));
    assert_eq!(ClipCell::text("-7").as_number(), Some(-7.0));
    assert_eq!(ClipCell::text("hello").as_number(), None);
    assert_eq!(ClipCell::text("").as_number(), None);
}

#[test]
fn paste_modes_agree_about_what_they_write() {
    assert!(PasteWhat::All.writes_contents() && PasteWhat::All.writes_formats());
    assert!(PasteWhat::Values.writes_contents() && !PasteWhat::Values.writes_formats());
    assert!(PasteWhat::Formulas.writes_contents() && !PasteWhat::Formulas.writes_formats());
    assert!(!PasteWhat::Formats.writes_contents() && PasteWhat::Formats.writes_formats());
    // Column Widths touches neither contents nor formats — that is the whole
    // point of it being a separate mode.
    assert!(!PasteWhat::ColumnWidths.writes_contents());
    assert!(!PasteWhat::ColumnWidths.writes_formats());
    assert!(PasteWhat::ColumnWidths.writes_widths());
    assert!(!PasteWhat::All.writes_widths());
}

#[test]
fn a_plain_paste_is_not_special_and_describes_itself_when_it_is() {
    assert!(!PasteOptions::plain().is_special());
    let o = PasteOptions {
        what: PasteWhat::Values,
        op: PasteOp::Add,
        transpose: true,
        skip_blanks: true,
    };
    assert!(o.is_special());
    let d = o.describe();
    for part in ["Values", "Add", "Transpose", "Skip Blanks"] {
        assert!(d.contains(part), "{part} missing from {d:?}");
    }
}

// -------------------------------------------------------------- rectangles --

#[test]
fn a_uniform_format_over_a_big_block_collapses_to_one_rectangle() {
    // THE scale invariant for a formatted paste. 100k cells of one format must
    // become ONE range entry; per-cell storage is exactly what format.rs
    // exists to avoid.
    let (rows, cols) = (1000usize, 100usize);
    let keys: Vec<Option<u8>> = vec![Some(7); rows * cols];
    let rects = merge_rectangles(&keys, rows, cols);
    assert_eq!(
        rects.len(),
        1,
        "100k uniformly formatted cells must be one rectangle, not {}",
        rects.len()
    );
    assert_eq!(rects[0].cells(), rows * cols, "and it must cover them all");
}

#[test]
fn rectangles_cover_exactly_the_non_empty_cells_and_do_not_overlap() {
    // Pins the property, not a hand-drawn expected list: every Some cell is
    // covered exactly once, every None cell is covered zero times.
    let (rows, cols) = (5usize, 4usize);
    #[rustfmt::skip]
    let keys: Vec<Option<u8>> = vec![
        Some(1), Some(1), None,    Some(2),
        Some(1), Some(1), None,    Some(2),
        None,    None,    Some(3), Some(2),
        Some(3), Some(3), Some(3), None,
        Some(3), Some(3), Some(3), None,
    ];
    let rects = merge_rectangles(&keys, rows, cols);
    let mut cover = vec![0usize; rows * cols];
    for r in &rects {
        for rr in r.first_row..=r.last_row {
            for cc in r.first_col..=r.last_col {
                cover[rr * cols + cc] += 1;
                assert_eq!(
                    keys[rr * cols + cc].as_ref(),
                    keys[r.key_index].as_ref(),
                    "rectangle at ({rr},{cc}) carries the wrong key"
                );
            }
        }
    }
    for (i, k) in keys.iter().enumerate() {
        let want = if k.is_some() { 1 } else { 0 };
        assert_eq!(
            cover[i], want,
            "cell {i} covered {} times, expected {want}",
            cover[i]
        );
    }
    // And it must actually be merging: 14 non-empty cells in far fewer rects.
    assert!(
        rects.len() < 8,
        "greedy merge did nothing useful: {} rectangles",
        rects.len()
    );
}

#[test]
fn different_keys_never_share_a_rectangle() {
    let keys: Vec<Option<u8>> = vec![Some(1), Some(2), Some(1), Some(2)];
    let rects = merge_rectangles(&keys, 2, 2);
    assert_eq!(rects.len(), 2, "two columns of two different keys");
    for r in &rects {
        assert_eq!(r.first_col, r.last_col, "a rectangle spans one key only");
    }
}

#[test]
fn an_empty_grid_produces_no_rectangles() {
    let keys: Vec<Option<u8>> = vec![None; 12];
    assert!(merge_rectangles(&keys, 3, 4).is_empty());
}

// ---------------------------------------------------------------- plumbing --

#[test]
fn a_block_reports_its_own_shape_honestly() {
    let b = plain(&[&["a", "b", "c"], &["d", "e", "f"]]);
    assert_eq!(b.cell_count(), 6);
    assert!(!b.is_empty());
    assert!(ClipBlock::new(0, 0).is_empty());
    assert!(b.get(2, 0).is_none(), "out of bounds reads are None");
    assert!(b.get(0, 3).is_none());
}

#[test]
fn to_text_grid_matches_what_went_in() {
    let grid = vec![
        vec!["a".to_string(), "".to_string()],
        vec!["c".to_string(), "d".to_string()],
    ];
    assert_eq!(ClipBlock::from_text_grid(&grid).to_text_grid(), grid);
}

#[test]
fn a_plain_cell_writes_no_attributes() {
    // Keeps the common case small: a 100k-cell plain copy must not carry
    // 100k empty style attributes.
    let html = to_html(&plain(&[&["a"]]));
    assert!(html.contains("<td>a</td>"), "got {html}");
}
