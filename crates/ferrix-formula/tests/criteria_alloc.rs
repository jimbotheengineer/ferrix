//! The scale invariant, enforced.
//!
//! Ferrix targets 200M+ rows. A conditional aggregate that allocated even one
//! `String` per row — to case-fold a cell, say, or to format a number for
//! comparison — would turn a 200M-row `COUNTIFS` into 200M heap round-trips
//! and a memory profile bounded by row count instead of by the viewport.
//!
//! So this test does not measure time. It counts allocations through a
//! wrapping global allocator and asserts that the *per-row* count is exactly
//! zero: doubling the number of rows must not change the allocation total at
//! all. That is a much sharper instrument than a timing threshold, and it
//! fails loudly the moment someone reaches for `to_lowercase()` inside the
//! matcher.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use ferrix_core::{CellRef, Sheet, Value};
use ferrix_formula::{eval, parse};

struct Counting;

// Counting is THREAD-LOCAL, not global. cargo runs test functions on parallel
// threads sharing one allocator; a global counter would tally every other
// test's allocations into this one's measurement and make the result depend
// on scheduling. Both cells are const-initialised so touching them from
// inside `alloc` cannot itself allocate.
thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

#[inline]
fn tick() {
    // `try_with` because TLS may already be torn down late in a thread's life;
    // an allocation there is not part of any measurement.
    let armed = ARMED.try_with(|a| a.get()).unwrap_or(false);
    if armed {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        tick();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        tick();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Run `f` with allocation counting on, returning the number of allocations
/// made on THIS thread while it ran.
fn allocations_during<T>(f: impl FnOnce() -> T) -> (T, u64) {
    ALLOCS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, ALLOCS.with(|c| c.get()))
}

/// A synthetic three-column sheet: region text, a number, and a value.
///
/// Interned text means every row's `region` shares one of three arena
/// strings, so the matcher has real (not empty) text to fold on every row.
fn synthetic(rows: u32) -> Sheet {
    let mut s = Sheet::new("big");
    let regions = ["North", "South", "Northeast"];
    for r in 0..rows {
        s.set_text(CellRef::new(r, 0), regions[(r % 3) as usize]);
        s.set(CellRef::new(r, 1), Value::Number((r % 200) as f64));
        s.set(CellRef::new(r, 2), Value::Number(1.0));
    }
    s
}

#[test]
fn multi_criteria_scan_does_not_allocate_per_row() {
    // Two sizes, 4x apart. If anything allocated per row, the larger scan
    // would allocate roughly four times as much.
    const SMALL: u32 = 25_000;
    const LARGE: u32 = 100_000;

    let small = synthetic(SMALL);
    let large = synthetic(LARGE);

    // Three criteria: a wildcard text match, a numeric comparison, and a
    // not-equal — i.e. every branch of the matcher, on every row.
    let f = format!(
        r#"=SUMIFS(C1:C{n},A1:A{n},"Nor*",B1:B{n},">=100",A1:A{n},"<>South")"#,
        n = LARGE
    );
    let expr_large = parse(&f).unwrap();
    let expr_small = parse(&f.replace(&LARGE.to_string(), &SMALL.to_string())).unwrap();

    // Warm up first: the very first eval may touch lazily-initialised state
    // that is legitimately one-off and not per-row.
    let _ = eval(&expr_small, &small);
    let _ = eval(&expr_large, &large);

    let (got_small, alloc_small) = allocations_during(|| eval(&expr_small, &small));
    let (got_large, alloc_large) = allocations_during(|| eval(&expr_large, &large));

    // Sanity: the scan actually did the work we think it did.
    // rows where region starts with "Nor" and is not "South" -> r % 3 != 1,
    // and r % 200 >= 100.
    let expect = |n: u32| -> f64 { (0..n).filter(|r| r % 3 != 1 && r % 200 >= 100).count() as f64 };
    assert_eq!(got_small, Value::Number(expect(SMALL)));
    assert_eq!(got_large, Value::Number(expect(LARGE)));

    // The real assertion: identical allocation counts despite 4x the rows.
    assert_eq!(
        alloc_small,
        alloc_large,
        "SUMIFS allocated {alloc_small} times over {SMALL} rows but {alloc_large} \
         times over {LARGE} rows — that is {} extra allocations for {} extra \
         rows, i.e. the scan allocates per row and peak memory now scales with \
         row count instead of viewport size",
        alloc_large.saturating_sub(alloc_small),
        LARGE - SMALL
    );

    // And it is a small constant, not a large one: compiling three criteria
    // plus the two spec vectors. Anything much bigger means per-chunk work
    // crept in.
    assert!(
        alloc_large <= 32,
        "SUMIFS made {alloc_large} allocations; expected a handful of \
         setup allocations bounded by the criteria count, not the data"
    );
}

#[test]
fn countif_wildcard_scan_does_not_allocate_per_row() {
    // COUNTIF takes a different code path from COUNTIFS (no spec vectors at
    // all), so pin it separately — and expect literally zero allocations.
    const SMALL: u32 = 20_000;
    const LARGE: u32 = 80_000;
    let small = synthetic(SMALL);
    let large = synthetic(LARGE);

    let e_small = parse(&format!(r#"=COUNTIF(A1:A{SMALL},"*th*")"#)).unwrap();
    let e_large = parse(&format!(r#"=COUNTIF(A1:A{LARGE},"*th*")"#)).unwrap();

    let _ = eval(&e_small, &small);
    let _ = eval(&e_large, &large);

    let (v_small, a_small) = allocations_during(|| eval(&e_small, &small));
    let (v_large, a_large) = allocations_during(|| eval(&e_large, &large));

    // Every region contains "th" (North / South / Northeast).
    assert_eq!(v_small, Value::Number(SMALL as f64));
    assert_eq!(v_large, Value::Number(LARGE as f64));

    assert_eq!(
        a_small, a_large,
        "COUNTIF allocation count moved with row count ({a_small} -> {a_large})"
    );
    assert!(
        a_large <= 8,
        "COUNTIF made {a_large} allocations scanning {LARGE} rows; the \
         criterion should compile once and match allocation-free"
    );
}

#[test]
fn criteria_matching_itself_allocates_nothing() {
    // The matcher in isolation, below the evaluator: compile once, then match
    // a million times for exactly zero allocations. This is the property
    // SUBSTITUTE/SEARCH will inherit when they reuse this module.
    use ferrix_formula::{Criterion, Scalar};

    let crits: Vec<Criterion> = ["Nor*", ">=100", "<>South", "a?c", "", "<>"]
        .iter()
        .map(|c| Criterion::parse(c))
        .collect();
    let cells = [
        Scalar::Text("Northeast"),
        Scalar::Text("south"),
        Scalar::Number(150.0),
        Scalar::Number(3.0),
        Scalar::Blank,
        Scalar::Text("abc"),
    ];

    let (hits, allocs) = allocations_during(|| {
        let mut hits = 0u64;
        for _ in 0..40_000 {
            for c in &crits {
                for cell in cells {
                    if c.matches(cell) {
                        hits += 1;
                    }
                }
            }
        }
        hits
    });

    assert!(hits > 0, "the matcher matched nothing; test is vacuous");
    assert_eq!(
        allocs, 0,
        "Criterion::matches allocated {allocs} times over 1.4M matches; it \
         must be allocation-free for a 200M-row scan to stay bounded"
    );
}
