//! Tests for page setup and pagination.
//!
//! The load-bearing test here is [`closed_form_matches_a_brute_force_walk`].
//! The paginator's whole reason for existing is that it does arithmetic per
//! uniform run instead of iterating rows, and the risk that buys is an
//! off-by-one in the arithmetic that no amount of eyeballing finds. So it is
//! checked against the naive row-at-a-time implementation across a spread of
//! shapes: if the two ever disagree about which page a row lands on, the fast
//! path is wrong.

use super::*;
use crate::sizing::{ColSizes, RowSizes};

fn setup_for(cap_rows: u32) -> PageSetup {
    // Margins chosen so the printable height is exactly cap_rows * 15pt.
    let mut s = PageSetup {
        gridlines: true,
        ..Default::default()
    };
    let (_, ph) = s.paper.dimensions();
    let want = cap_rows as f32 * DEFAULT_ROW_HEIGHT;
    let slack = ph - want;
    s.margins = Margins {
        left: 0.0,
        right: 0.0,
        top: slack / 2.0,
        bottom: slack / 2.0,
        header: 0.0,
        footer: 0.0,
    };
    s
}

/// Map every visible row to the index of the band it lands in.
fn row_to_band(bands: &[(u32, u32)], sizes: &RowSizes, first: u32, last: u32) -> Vec<(u32, usize)> {
    let mut out = Vec::new();
    for r in first..=last {
        if sizes.height_or(r, DEFAULT_ROW_HEIGHT) <= 0.0 {
            continue;
        }
        if let Some(i) = bands.iter().position(|(a, b)| r >= *a && r <= *b) {
            out.push((r, i));
        }
    }
    out
}

/// The naive paginator: walk one row at a time, cut when the page overflows.
fn brute_force_bands(
    sizes: &RowSizes,
    first: u32,
    last: u32,
    cap: Points,
    breaks: &[u32],
) -> Vec<(u32, u32)> {
    let mut bands = Vec::new();
    let mut start: Option<u32> = None;
    let mut used = 0.0f32;
    let mut last_placed = first;
    for r in first..=last {
        let h = sizes.height_or(r, DEFAULT_ROW_HEIGHT);
        if h <= 0.0 {
            continue;
        }
        let forced = breaks.contains(&r) && start.is_some();
        if forced || (start.is_some() && used + h > cap) {
            bands.push((start.unwrap(), last_placed));
            start = None;
            used = 0.0;
        }
        if start.is_none() {
            start = Some(r);
        }
        used += h;
        last_placed = r;
        // A row taller than a whole page occupies it alone.
        if used > cap {
            bands.push((start.unwrap(), r));
            start = None;
            used = 0.0;
        }
    }
    if let Some(s) = start {
        bands.push((s, last_placed));
    }
    bands
}

#[test]
fn closed_form_matches_a_brute_force_walk() {
    // A spread of shapes: uniform, one odd row, hidden runs, mixed spans,
    // and capacities that divide evenly or leave a remainder.
    let shapes: Vec<(&str, Box<dyn Fn(&mut RowSizes)>, u32, u32, f32)> = vec![
        ("uniform", Box::new(|_: &mut RowSizes| {}), 0, 99, 150.0),
        (
            "uniform-exact-fit",
            Box::new(|_: &mut RowSizes| {}),
            0,
            99,
            75.0,
        ),
        (
            "one-tall-row",
            Box::new(|s: &mut RowSizes| s.set(7, 40.0)),
            0,
            50,
            150.0,
        ),
        (
            "row-taller-than-page",
            Box::new(|s: &mut RowSizes| s.set(3, 500.0)),
            0,
            20,
            150.0,
        ),
        (
            "hidden-run",
            Box::new(|s: &mut RowSizes| s.hide(10, 25)),
            0,
            60,
            150.0,
        ),
        (
            "mixed-spans",
            Box::new(|s: &mut RowSizes| {
                s.set_range(0, 9, 30.0);
                s.set_range(20, 29, 7.5);
                s.hide(40, 44);
            }),
            0,
            80,
            135.0,
        ),
        (
            "tiny-capacity",
            Box::new(|_: &mut RowSizes| {}),
            0,
            30,
            15.0,
        ),
        (
            "all-hidden-middle",
            Box::new(|s: &mut RowSizes| s.hide(5, 55)),
            0,
            60,
            45.0,
        ),
    ];

    for (name, build, first, last, cap) in shapes {
        let mut sizes = RowSizes::new();
        build(&mut sizes);

        let fast = band_rows(&sizes, (first, last), cap, &[]);
        let slow = brute_force_bands(&sizes, first, last, cap, &[]);

        let fast_map = row_to_band(&fast, &sizes, first, last);
        let slow_map = row_to_band(&slow, &sizes, first, last);

        assert_eq!(
            fast_map, slow_map,
            "shape {name:?}: closed-form pagination disagrees with the \
             row-at-a-time walk about which page rows land on\n  fast bands: \
             {fast:?}\n  slow bands: {slow:?}"
        );
        assert_eq!(
            fast.len(),
            slow.len(),
            "shape {name:?}: page count differs (fast {} vs brute {})",
            fast.len(),
            slow.len()
        );
    }
}

#[test]
fn manual_breaks_match_a_brute_force_walk() {
    let mut sizes = RowSizes::new();
    sizes.set_range(10, 19, 22.5);
    let breaks = vec![5u32, 17, 40];
    let fast = band_rows(&sizes, (0, 60), 150.0, &breaks);
    let slow = brute_force_bands(&sizes, 0, 60, 150.0, &breaks);
    assert_eq!(
        row_to_band(&fast, &sizes, 0, 60),
        row_to_band(&slow, &sizes, 0, 60),
        "manual breaks: fast {fast:?} vs brute {slow:?}"
    );
    // And the breaks really did force a cut: every break row starts a band.
    for b in &breaks {
        assert!(
            fast.iter().any(|(first, _)| first == b),
            "row {b} has a manual page break but does not start a band: {fast:?}"
        );
    }
}

#[test]
fn a_row_never_straddles_a_page() {
    // 150pt of capacity holds ten 15pt rows exactly; make one row 20pt so the
    // boundary lands mid-row if anything splits.
    let mut sizes = RowSizes::new();
    sizes.set(9, 20.0);
    let bands = band_rows(&sizes, (0, 99), 150.0, &[]);
    for (first, last) in &bands {
        let h: f32 = (*first..=*last)
            .map(|r| sizes.height_or(r, DEFAULT_ROW_HEIGHT))
            .sum();
        // Either the band fits, or it is a single row that cannot fit anywhere.
        assert!(
            h <= 150.0 + f32::EPSILON || first == last,
            "band {first}..={last} is {h}pt, over the 150pt page, and is not a \
             single oversized row — a row was split across the boundary"
        );
    }
    // Every row appears in exactly one band.
    for r in 0..=99u32 {
        let hits = bands.iter().filter(|(a, b)| r >= *a && r <= *b).count();
        assert_eq!(
            hits, 1,
            "row {r} appears in {hits} bands, expected exactly 1"
        );
    }
}

#[test]
fn a_row_taller_than_a_page_gets_its_own_page_instead_of_looping() {
    let mut sizes = RowSizes::new();
    sizes.set(4, 5000.0);
    let bands = band_rows(&sizes, (0, 9), 150.0, &[]);
    assert!(
        bands.iter().any(|(a, b)| *a == 4 && *b == 4),
        "the 5000pt row 4 should occupy a page alone, got {bands:?}"
    );
    // And the rows around it are still all placed.
    for r in 0..=9u32 {
        assert!(
            bands.iter().any(|(a, b)| r >= *a && r <= *b),
            "row {r} was dropped: {bands:?}"
        );
    }
}

#[test]
fn paginating_200_million_rows_is_arithmetic_not_iteration() {
    // The scale invariant, made falsifiable. A row-at-a-time paginator would
    // need 200M iterations here; this must return effectively instantly and
    // must not allocate a band per page.
    let mut sizes = RowSizes::new();
    sizes.set_range(1_000_000, 1_000_009, 60.0);

    let setup = setup_for(50);
    let cols = ColSizes::new();
    let start = std::time::Instant::now();
    let p = Paginator::new(setup, (0, 199_999_999), (0, 5), &sizes, &cols);
    let count = p.page_count();
    let elapsed = start.elapsed();

    assert!(
        count > 3_000_000,
        "200M rows at 50 per page should be about 4M pages, got {count}"
    );
    assert!(
        elapsed.as_millis() < 250,
        "pagination took {elapsed:?} — that is iteration over rows, not \
         arithmetic over runs"
    );

    // The proof it did not materialise: bands are O(runs), not O(pages).
    assert!(
        p.row_band_count() as u64 == count / p.col_band_count() as u64,
        "band bookkeeping inconsistent"
    );

    // And asking for the first few pages is lazy — this must not build 4M.
    let first_three: Vec<Page> = p.pages().take(3).collect();
    assert_eq!(first_three.len(), 3);
    assert_eq!(first_three[0].number, 1);
    assert_eq!(first_three[0].first_row, 0);
    assert_eq!(
        first_three[0].last_row, 49,
        "first page should hold 50 default-height rows"
    );
}

#[test]
fn a_large_job_is_flagged_before_anything_is_rendered() {
    let sizes = RowSizes::new();
    let cols = ColSizes::new();

    let small = Paginator::new(setup_for(50), (0, 99), (0, 3), &sizes, &cols);
    assert!(
        !small.is_large(),
        "a 2-page job should not warn, got {} pages",
        small.page_count()
    );

    let huge = Paginator::new(setup_for(50), (0, 9_999_999), (0, 3), &sizes, &cols);
    assert!(
        huge.is_large(),
        "a {}-page job must be flagged",
        huge.page_count()
    );
    assert!(huge.page_count() > LARGE_JOB_PAGES);
}

#[test]
fn repeated_header_rows_reduce_every_pages_capacity() {
    let sizes = RowSizes::new();
    let cols = ColSizes::new();

    let plain = Paginator::new(setup_for(10), (0, 99), (0, 3), &sizes, &cols);

    let mut with_repeat = setup_for(10);
    with_repeat.repeat_rows = Some((0, 2));
    let repeated = Paginator::new(with_repeat, (0, 99), (0, 3), &sizes, &cols);

    assert!(
        repeated.page_count() > plain.page_count(),
        "reserving 3 header rows on every page must cost pages: {} with \
         repeat vs {} without",
        repeated.page_count(),
        plain.page_count()
    );
    let first = repeated.pages().next().unwrap();
    assert_eq!(
        first.last_row, 6,
        "with 3 of 10 row-slots spent on the repeated header, 7 body rows \
         should fit, so the first page ends at row 6"
    );
}

#[test]
fn fit_to_width_shrinks_until_the_columns_fit_one_page() {
    let sizes = RowSizes::new();
    let mut cols = ColSizes::new();
    for c in 0..40u32 {
        cols.set_width(c, 100.0);
    }

    let mut unscaled = setup_for(40);
    unscaled.margins.left = 0.0;
    unscaled.margins.right = 0.0;
    let plain = Paginator::new(unscaled.clone(), (0, 99), (0, 39), &sizes, &cols);
    assert!(
        plain.col_band_count() > 1,
        "4000pt of columns must not fit one 612pt page unscaled"
    );

    let mut fit = unscaled;
    fit.scaling = Scaling::FitTo {
        wide: Some(1),
        tall: None,
    };
    let fitted = Paginator::new(fit, (0, 99), (0, 39), &sizes, &cols);
    assert_eq!(
        fitted.col_band_count(),
        1,
        "fit-to-1-page-wide must put all 40 columns in one column band, got {}",
        fitted.col_band_count()
    );
}

#[test]
fn fit_to_page_never_magnifies_small_content() {
    let s = Scaling::FitTo {
        wide: Some(1),
        tall: Some(1),
    };
    // Content is a quarter of the page; Excel leaves it a quarter.
    let f = s.factor((150.0, 200.0), (600.0, 800.0));
    assert!(
        (f - 1.0).abs() < 1e-6,
        "fit-to-page magnified a small report to {f}x; it should stay 1.0"
    );
}

#[test]
fn hidden_rows_and_columns_take_up_no_space_on_the_page() {
    let mut sizes = RowSizes::new();
    sizes.hide(0, 49);
    let cols = ColSizes::new();
    let p = Paginator::new(setup_for(10), (0, 59), (0, 3), &sizes, &cols);
    assert_eq!(
        p.row_band_count(),
        1,
        "50 hidden rows plus 10 visible ones is one page, got {} — hidden \
         rows are consuming page space",
        p.row_band_count()
    );

    let mut hidden_cols = ColSizes::new();
    for c in 0..20u32 {
        hidden_cols.set_width(c, 200.0);
    }
    for c in 0..18u32 {
        hidden_cols.hide(c);
    }
    let sizes2 = RowSizes::new();
    let mut wide = setup_for(10);
    wide.margins.left = 0.0;
    wide.margins.right = 0.0;
    let p2 = Paginator::new(wide, (0, 9), (0, 19), &sizes2, &hidden_cols);
    assert_eq!(
        p2.col_band_count(),
        1,
        "only 2 visible 200pt columns remain, which fit one 612pt page"
    );
}

#[test]
fn page_order_changes_numbering_but_not_the_set_of_pages() {
    let sizes = RowSizes::new();
    let mut cols = ColSizes::new();
    for c in 0..20u32 {
        cols.set_width(c, 200.0);
    }

    let mut down = setup_for(10);
    down.margins.left = 0.0;
    down.margins.right = 0.0;
    down.order = PageOrder::DownThenOver;
    let mut over = down.clone();
    over.order = PageOrder::OverThenDown;

    let a = Paginator::new(down, (0, 99), (0, 19), &sizes, &cols);
    let b = Paginator::new(over, (0, 99), (0, 19), &sizes, &cols);

    assert_eq!(a.page_count(), b.page_count());
    assert!(
        a.page_count() > 4,
        "need a genuine grid of pages to test order"
    );

    let mut set_a: Vec<_> = a
        .pages()
        .map(|p| (p.first_row, p.last_row, p.first_col, p.last_col))
        .collect();
    let mut set_b: Vec<_> = b
        .pages()
        .map(|p| (p.first_row, p.last_row, p.first_col, p.last_col))
        .collect();
    set_a.sort();
    set_b.sort();
    assert_eq!(set_a, set_b, "the two orders must cover the same pages");

    // But page 2 is a different part of the sheet in each.
    let p2a = a.pages().nth(1).unwrap();
    let p2b = b.pages().nth(1).unwrap();
    assert_ne!(
        (p2a.first_row, p2a.first_col),
        (p2b.first_row, p2b.first_col),
        "down-then-over and over-then-down should disagree about page 2"
    );
    assert_eq!(
        p2a.first_col, 0,
        "down-then-over continues down column band 0"
    );
    assert_eq!(
        p2b.first_row, 0,
        "over-then-down continues across row band 0"
    );
}

#[test]
fn every_page_number_is_unique_and_contiguous() {
    let sizes = RowSizes::new();
    let mut cols = ColSizes::new();
    for c in 0..12u32 {
        cols.set_width(c, 200.0);
    }
    let mut s = setup_for(10);
    s.margins.left = 0.0;
    s.margins.right = 0.0;
    let p = Paginator::new(s, (0, 55), (0, 11), &sizes, &cols);
    let nums: Vec<u64> = p.pages().map(|pg| pg.number).collect();
    let expected: Vec<u64> = (1..=p.page_count()).collect();
    assert_eq!(nums, expected, "page numbers must be 1..=n with no gaps");
}

#[test]
fn page_at_agrees_with_the_page_iterator() {
    let sizes = RowSizes::new();
    let mut cols = ColSizes::new();
    for c in 0..12u32 {
        cols.set_width(c, 200.0);
    }
    let mut s = setup_for(10);
    s.margins.left = 0.0;
    s.margins.right = 0.0;
    let p = Paginator::new(s, (0, 55), (0, 11), &sizes, &cols);

    for page in p.pages() {
        let found = p
            .page_at(page.first_row, page.first_col)
            .expect("every page's own corner must resolve to that page");
        assert_eq!(
            found, page,
            "page_at({}, {}) returned a different page than the iterator",
            page.first_row, page.first_col
        );
    }
}

#[test]
fn header_fields_resolve_to_their_values() {
    let ctx = FieldContext {
        page: 3,
        pages: 17,
        date: "2026-08-29".into(),
        time: "22:15".into(),
        file: "sales.ferrix".into(),
        sheet: "Q3".into(),
    };
    assert_eq!(substitute_fields("Page &P of &N", &ctx), "Page 3 of 17");
    assert_eq!(substitute_fields("&F / &A", &ctx), "sales.ferrix / Q3");
    assert_eq!(substitute_fields("&D &T", &ctx), "2026-08-29 22:15");
    // A literal ampersand.
    assert_eq!(substitute_fields("R&&D", &ctx), "R&D");
    // Unknown codes survive rather than being silently eaten.
    assert_eq!(
        substitute_fields("&Q&Z", &ctx),
        "&Q&Z",
        "an unrecognised field code must be left visible so the user can see \
         their typo, not deleted"
    );
    // A trailing bare ampersand.
    assert_eq!(substitute_fields("total &", &ctx), "total &");
    assert!(substitute_fields("", &ctx).is_empty());
}

#[test]
fn header_footer_renders_all_three_parts() {
    let hf = HeaderFooter {
        left: "&F".into(),
        center: "Confidential".into(),
        right: "&P/&N".into(),
    };
    let ctx = FieldContext {
        page: 2,
        pages: 9,
        file: "book.ferrix".into(),
        ..Default::default()
    };
    assert_eq!(
        hf.render(&ctx),
        [
            "book.ferrix".to_string(),
            "Confidential".to_string(),
            "2/9".to_string()
        ]
    );
    assert!(HeaderFooter::default().is_empty());
    assert!(!hf.is_empty());
}

#[test]
fn orientation_swaps_the_page_dimensions() {
    let mut s = PageSetup::default();
    let (pw, ph) = s.paper_size();
    assert!(ph > pw, "Letter portrait is taller than it is wide");
    s.orientation = Orientation::Landscape;
    let (lw, lh) = s.paper_size();
    assert_eq!((lw, lh), (ph, pw), "landscape must swap width and height");
}

#[test]
fn landscape_fits_more_columns_and_fewer_rows() {
    let sizes = RowSizes::new();
    let mut cols = ColSizes::new();
    for c in 0..20u32 {
        cols.set_width(c, 100.0);
    }
    let portrait = PageSetup {
        margins: Margins {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
            header: 0.0,
            footer: 0.0,
        },
        ..Default::default()
    };
    let mut landscape = portrait.clone();
    landscape.orientation = Orientation::Landscape;

    let p = Paginator::new(portrait, (0, 199), (0, 19), &sizes, &cols);
    let l = Paginator::new(landscape, (0, 199), (0, 19), &sizes, &cols);

    assert!(
        l.col_band_count() < p.col_band_count(),
        "landscape should need fewer column bands ({} vs {})",
        l.col_band_count(),
        p.col_band_count()
    );
    assert!(
        l.row_band_count() > p.row_band_count(),
        "landscape is shorter so it should need more row bands ({} vs {})",
        l.row_band_count(),
        p.row_band_count()
    );
}

#[test]
fn absurd_margins_produce_an_empty_page_not_a_hang() {
    let mut s = PageSetup::default();
    s.margins = Margins {
        left: 5000.0,
        right: 5000.0,
        top: 5000.0,
        bottom: 5000.0,
        header: 0.0,
        footer: 0.0,
    };
    let (w, h) = s.printable();
    assert_eq!((w, h), (0.0, 0.0), "printable area must clamp at zero");

    // The real risk is a zero capacity making the paginator loop forever.
    let sizes = RowSizes::new();
    let cols = ColSizes::new();
    let p = Paginator::new(s, (0, 20), (0, 3), &sizes, &cols);
    assert!(p.page_count() >= 1, "must still produce at least one page");
    assert_eq!(
        p.pages().count() as u64,
        p.page_count(),
        "the iterator must agree with the count"
    );
}

#[test]
fn manual_breaks_stay_sorted_and_deduplicated() {
    let mut s = PageSetup::default();
    s.add_row_break(40);
    s.add_row_break(10);
    s.add_row_break(25);
    s.add_row_break(10);
    assert_eq!(s.row_breaks, vec![10, 25, 40]);
    assert!(s.remove_row_break(25));
    assert!(!s.remove_row_break(25));
    assert_eq!(s.row_breaks, vec![10, 40]);

    s.add_col_break(3);
    s.add_col_break(1);
    s.add_col_break(3);
    assert_eq!(s.col_breaks, vec![1, 3]);
    assert!(s.remove_col_break(1));
    assert_eq!(s.col_breaks, vec![3]);
}

#[test]
fn break_preview_reports_the_rows_where_pages_split() {
    let sizes = RowSizes::new();
    let cols = ColSizes::new();
    let p = Paginator::new(setup_for(10), (0, 44), (0, 3), &sizes, &cols);
    let breaks: Vec<u32> = p.row_break_rows().collect();
    assert_eq!(
        breaks,
        vec![10, 20, 30, 40],
        "45 rows at 10 per page break before rows 10, 20, 30 and 40"
    );
    // The first page never counts as a break.
    assert!(!breaks.contains(&0));
}

#[test]
fn measuring_a_range_sums_only_visible_rows() {
    let mut sizes = RowSizes::new();
    sizes.set_range(0, 9, 20.0);
    sizes.hide(3, 5);
    // 10 rows at 20pt, minus 3 hidden = 7 * 20.
    assert_eq!(measure_rows(&sizes, 0, 9), 140.0);
    assert_eq!(
        measure_rows(&sizes, 5, 4),
        0.0,
        "an inverted range is empty"
    );

    let mut cols = ColSizes::new();
    cols.set_width(0, 30.0);
    cols.set_width(1, 70.0);
    cols.hide(1);
    // col 0 explicit, col 1 hidden, col 2 default.
    assert_eq!(measure_cols(&cols, 0, 2), 30.0 + DEFAULT_COL_WIDTH);
}

#[test]
fn paper_sizes_are_all_positive_and_portrait() {
    for p in PaperSize::all() {
        let (w, h) = p.dimensions();
        assert!(w > 0.0 && h > 0.0, "{:?} has a non-positive dimension", p);
        assert!(
            h > w,
            "{:?} is defined portrait, so height must exceed width",
            p
        );
        assert!(!p.label().is_empty());
    }
}

#[test]
fn a_page_is_small_enough_to_stream() {
    // The type is produced lazily one at a time; keep it cheap enough that
    // that stays true if someone later collects a bounded window of them.
    assert!(
        std::mem::size_of::<Page>() <= 32,
        "Page grew to {} bytes",
        std::mem::size_of::<Page>()
    );
}
