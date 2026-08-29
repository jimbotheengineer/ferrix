//! Proof that formatting cost is independent of row count.
//!
//! This is the acceptance test for the storage constraint in issue #18:
//! *formatting a 200M-row column must allocate nothing per row*. It is an
//! integration test rather than a unit test because it installs a global
//! counting allocator, which a `#[cfg(test)]` module inside the library
//! cannot do without affecting every other test in the crate.
//!
//! Three independent claims are checked, because "costs nothing per row" can
//! fail in three different ways:
//!
//! 1. **Storage** — configuring a rule over 200M rows must allocate the same
//!    bytes as configuring it over 200. A `HashMap<CellRef, Style>`
//!    implementation fails here by ~9.6 GB.
//! 2. **Resolution** — painting a viewport of cells must allocate *zero*
//!    bytes, and the same number of bytes, whatever the column's height.
//!    An implementation that materialises styles lazily but caches them per
//!    cell fails here.
//! 3. **Time** — resolving a viewport must take time proportional to the
//!    viewport, not the column. An implementation that scans the column to
//!    find its extent fails here, and only here.
//!
//! The allocator counts *bytes allocated*, not net heap, so a transient
//! allocation that is immediately freed still fails the zero-allocation
//! assertions. That is deliberate: transient per-cell allocation is exactly
//! the frame-budget problem this design exists to avoid.
//!
//! Counting is **per thread**. Cargo runs tests in parallel, so a global
//! counter would attribute another test's allocations to whichever test
//! happened to be measuring — which is not a hypothetical, it is what the
//! first draft of this file did.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

use ferrix_core::format::presets;
use ferrix_core::{
    CellRef, ManualStyle, NumberFormat, PlanEntry, RangeFormat, Rgb, RuleEval, SheetFormat,
    TableRange, Value,
};

// ============================================================== the allocator ==

struct Counting;

thread_local! {
    /// Bytes allocated on this thread while counting was on.
    static BYTES: Cell<usize> = const { Cell::new(0) };
    /// Whether this thread is currently measuring.
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// Add to this thread's counter, tolerating TLS teardown.
///
/// `try_with` rather than `with`: an allocation can happen while thread-local
/// destructors are running, and panicking inside the global allocator would
/// abort the process.
#[inline]
fn note(bytes: usize) {
    let on = ENABLED.try_with(Cell::get).unwrap_or(false);
    if on {
        let _ = BYTES.try_with(|b| b.set(b.get() + bytes));
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size.saturating_sub(layout.size()));
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Run `f` with allocation counting on, returning the bytes it allocated on
/// this thread.
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    BYTES.with(|b| b.set(0));
    ENABLED.with(|e| e.set(true));
    let out = f();
    ENABLED.with(|e| e.set(false));
    (out, BYTES.with(Cell::get))
}

/// The two row counts every claim is tested at. Six orders of magnitude apart.
const SMALL: u32 = 200;
const HUGE: u32 = 200_000_000;

/// A realistic viewport: ~28 rows x 10 columns is what the grid paints.
const VIEW_ROWS: u32 = 28;
const VIEW_COLS: u32 = 10;

/// Build the formatting a user would plausibly configure on one column:
/// a manual tint, sign colouring, a threshold, and a number format.
fn configure(rows: u32) -> SheetFormat {
    let mut f = SheetFormat::new();
    f.set_column_manual(
        0,
        ManualStyle {
            fill: Some(Rgb(0xF0, 0xF0, 0xF8)),
            text: None,
            typography: Default::default(),
        },
    );
    f.push_column_rule(0, presets::sign_colors());
    f.push_column_rule(0, presets::above(1000.0));
    f.set_column_format(
        0,
        NumberFormat::Currency {
            symbol: "$".into(),
            places: 2,
        },
    );
    // Plus a range rule spanning the whole column, so range scope is exercised
    // at both sizes too.
    f.push_range(
        RangeFormat::new(TableRange::new(0, 0, rows.saturating_sub(1), 0))
            .with_rule(presets::below(-1000.0)),
    );
    f
}

// ================================================================ 1. storage ==

#[test]
fn configuring_a_200m_row_column_allocates_exactly_what_a_200_row_one_does() {
    let (small, small_bytes) = measure(|| configure(SMALL));
    let (huge, huge_bytes) = measure(|| configure(HUGE));

    assert_eq!(
        small_bytes, huge_bytes,
        "configuring the same rules over {HUGE} rows allocated {huge_bytes} bytes \
         but over {SMALL} rows allocated {small_bytes}; formatting cost must not \
         scale with row count"
    );
    assert_eq!(
        small.heap_bytes(),
        huge.heap_bytes(),
        "resident heap must not scale with row count either"
    );
    // Sanity: the rules really are all there, so this is not passing by
    // storing nothing.
    assert_eq!(huge.rule_count(), 4);
    assert_eq!(huge.override_count(), 0, "no per-cell entries were created");

    // And the absolute number is small — a few hundred bytes, not gigabytes.
    // A HashMap<CellRef, Style> over 200M rows would be ~9.6 GB here.
    assert!(
        huge.heap_bytes() < 4096,
        "the whole configuration should be well under 4 KiB, was {}",
        huge.heap_bytes()
    );
}

#[test]
fn colouring_a_whole_column_costs_one_rule_not_one_entry_per_row() {
    let mut f = SheetFormat::new();
    let (_, bytes) = measure(|| {
        f.set_column_manual(
            0,
            ManualStyle {
                fill: Some(Rgb(255, 240, 0)),
                text: None,
                typography: Default::default(),
            },
        );
    });
    // The bound is a couple of KiB, not a couple of hundred bytes, because a
    // `BTreeMap` allocates a whole node (capacity 11) on first insert. That is
    // a fixed cost paid once for the first eleven formatted columns — it does
    // not grow with rows, and the next ten columns allocate nothing at all.
    assert!(
        bytes < 2048,
        "colouring an entire 200M-row column allocated {bytes} bytes; it should \
         be a single rule plus at most one map node"
    );

    // The claim that actually matters: cost is per *column*, and small. Nine
    // more columns cost nine more rule vectors — a constant each — and nothing
    // that scales with the 200M rows any of them covers.
    let (_, more) = measure(|| {
        for col in 1..10u32 {
            f.set_column_manual(
                col,
                ManualStyle {
                    fill: Some(Rgb(255, 240, 0)),
                    text: None,
                    typography: Default::default(),
                },
            );
        }
    });
    let per_column = more / 9;
    assert!(
        per_column < 256,
        "each additional formatted column cost {per_column} bytes; a column rule \
         must be a constant, not a function of the rows it covers"
    );

    // And the rule genuinely reaches the far end of the column.
    let mut plan: Vec<PlanEntry<'_>> = Vec::new();
    f.plan(0, &mut plan);
    let s = f.resolve(
        CellRef::new(HUGE - 1, 0),
        &Value::Number(1.0),
        "",
        &plan,
        &[],
    );
    assert_eq!(s.fill, Some(Rgb(255, 240, 0)));
}

#[test]
fn a_selection_range_of_any_size_is_a_single_entry() {
    let mut small = SheetFormat::new();
    let (_, small_bytes) = measure(|| {
        small.set_range_manual(
            TableRange::new(0, 0, SMALL, 4),
            ManualStyle {
                fill: Some(Rgb(1, 2, 3)),
                text: None,
                typography: Default::default(),
            },
        );
    });
    let mut huge = SheetFormat::new();
    let (_, huge_bytes) = measure(|| {
        huge.set_range_manual(
            TableRange::new(0, 0, HUGE, 4),
            ManualStyle {
                fill: Some(Rgb(1, 2, 3)),
                text: None,
                typography: Default::default(),
            },
        );
    });
    assert_eq!(small_bytes, huge_bytes);
    assert_eq!(huge.ranges().len(), 1);
}

// ============================================================= 2. resolution ==

/// Resolve a full viewport the way the grid does: plan once per column, then
/// walk the cells.
fn paint_viewport<'a>(f: &'a SheetFormat, first_row: u32, buf: &mut Vec<PlanEntry<'a>>) -> usize {
    let mut styled = 0;
    for col in 0..VIEW_COLS {
        f.plan(col, buf);
        if buf.is_empty() {
            continue;
        }
        for row in first_row..first_row + VIEW_ROWS {
            let c = CellRef::new(row, col);
            let v = Value::Number(((row as f64) % 17.0) - 8.0);
            let s = f.resolve(c, &v, "", buf, &[]);
            if !s.is_plain() {
                styled += 1;
            }
        }
    }
    styled
}

#[test]
fn painting_a_viewport_allocates_nothing_at_any_row_count() {
    let f = configure(HUGE);
    // Warm the plan buffer outside the measurement: its one-time growth is a
    // per-frame-buffer cost, not a per-cell cost, and the grid owns it across
    // frames exactly like this.
    let mut buf: Vec<PlanEntry<'_>> = Vec::with_capacity(16);
    paint_viewport(&f, 0, &mut buf);

    // Near the top of the column...
    let (near, near_bytes) = measure(|| paint_viewport(&f, 0, &mut buf));
    // ...and 199,999,000 rows in.
    let (far, far_bytes) = measure(|| paint_viewport(&f, HUGE - 1_000, &mut buf));

    assert_eq!(
        near_bytes, 0,
        "resolving a viewport must allocate nothing; allocated {near_bytes} bytes"
    );
    assert_eq!(
        far_bytes, 0,
        "resolving a viewport 200M rows down must allocate nothing; allocated \
         {far_bytes} bytes"
    );
    assert!(
        near > 0 && far > 0,
        "the test must actually be styling cells"
    );
}

#[test]
fn resolving_one_cell_allocates_nothing_even_with_every_rule_kind_active() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::sign_colors());
    f.push_column_rule(0, presets::above(10.0));
    f.push_column_rule(0, presets::below(-10.0));
    f.push_column_rule(0, presets::color_scale());
    f.push_column_rule(0, presets::data_bar());
    f.push_column_rule(0, presets::top_n(5));
    f.push_column_rule(0, presets::contains("warn"));

    let mut plan: Vec<PlanEntry<'_>> = Vec::new();
    f.plan(0, &mut plan);
    let evals = vec![
        RuleEval {
            extent: Some((-100.0, 100.0)),
            cut: Some(50.0),
        };
        plan.len()
    ];

    let (style, bytes) = measure(|| {
        f.resolve(
            CellRef::new(HUGE - 1, 0),
            &Value::Number(64.0),
            "a warning line",
            &plan,
            &evals,
        )
    });
    assert_eq!(
        bytes, 0,
        "resolve() must be allocation-free even with 7 rules; allocated {bytes}"
    );
    assert!(!style.is_plain(), "the rules must actually have applied");
}

#[test]
fn a_number_format_is_borrowed_not_cloned_per_cell() {
    let mut f = SheetFormat::new();
    f.set_column_format(
        0,
        NumberFormat::Currency {
            symbol: "USD ".into(),
            places: 2,
        },
    );
    let (fmt, bytes) = measure(|| {
        // Resolve the format for a whole viewport's worth of cells.
        let mut last = None;
        for row in 0..VIEW_ROWS {
            last = f.number_format(CellRef::new(HUGE - row - 1, 0));
        }
        last.is_some()
    });
    assert!(fmt, "the format must resolve");
    assert_eq!(
        bytes, 0,
        "looking up a number format must not clone its currency symbol; \
         allocated {bytes} bytes over {VIEW_ROWS} cells"
    );
}

// =================================================================== 3. time ==

#[test]
fn viewport_resolution_time_does_not_grow_with_the_column() {
    let small = configure(SMALL);
    let huge = configure(HUGE);
    // Warm both paths so neither pays first-touch costs. Each gets its own
    // buffer: a plan borrows its rules, so one buffer cannot serve two stores.
    paint_viewport(&small, 0, &mut Vec::with_capacity(16));
    paint_viewport(&huge, 0, &mut Vec::with_capacity(16));

    const REPS: u32 = 200;
    let t_small = {
        let mut buf = Vec::with_capacity(16);
        let t = Instant::now();
        for i in 0..REPS {
            std::hint::black_box(paint_viewport(&small, i % 100, &mut buf));
        }
        t.elapsed()
    };
    let t_huge = {
        let mut buf = Vec::with_capacity(16);
        let t = Instant::now();
        for i in 0..REPS {
            std::hint::black_box(paint_viewport(&huge, HUGE - 1_000 + (i % 100), &mut buf));
        }
        t.elapsed()
    };

    // A row-count-dependent implementation would be a million times slower
    // here, not 4x. The bound is loose on purpose: this test must fail on an
    // algorithmic regression and never on a noisy CI machine.
    let ratio = t_huge.as_secs_f64() / t_small.as_secs_f64().max(1e-9);
    assert!(
        ratio < 4.0,
        "painting a viewport of a {HUGE}-row column took {ratio:.2}x as long as \
         a {SMALL}-row one ({t_huge:?} vs {t_small:?}); cost must be independent \
         of row count"
    );
}

#[test]
fn a_full_viewport_resolves_far_inside_the_frame_budget() {
    // The claim is about the formatting layer only — this measures rule
    // resolution, not egui's painting, so it is a lower bound on the frame
    // rather than a full frame timing.
    let f = configure(HUGE);
    let mut buf: Vec<PlanEntry<'_>> = Vec::with_capacity(16);
    paint_viewport(&f, 0, &mut buf);

    const REPS: u32 = 100;
    let t = Instant::now();
    for i in 0..REPS {
        std::hint::black_box(paint_viewport(&f, HUGE - 1_000 + i, &mut buf));
    }
    let per_frame = t.elapsed() / REPS;

    // 16.67 ms is the 60fps budget; formatting is allowed a small slice of it.
    assert!(
        per_frame.as_secs_f64() * 1000.0 < 1.0,
        "resolving {} cells took {per_frame:?} per frame; formatting must be a \
         small fraction of the 16.67 ms budget",
        VIEW_ROWS * VIEW_COLS
    );
}
