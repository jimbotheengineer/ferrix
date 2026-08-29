//! The lookup family's scale invariant, enforced by counting allocations.
//!
//! `src/lookup/tests.rs` already counts the cells a lookup VISITS, which is
//! what proves it does not scan a 10M-row column. This file pins the other
//! half of the same invariant: that a lookup does not *allocate* per cell it
//! visits. The two failures are genuinely different —
//!
//! * an implementation that binary-searches but calls `to_lowercase()` on
//!   every probed cell passes the visit-count test and fails this one;
//! * an implementation that allocates nothing but collects the column into a
//!   `Vec<Value>` first fails the visit-count test and could pass this one
//!   (one big allocation, not one per row).
//!
//! Together they leave nowhere for a materialising implementation to hide.
//!
//! **Every scan here is made to run the FULL length of the column**, by
//! probing for a key the data does not contain. A test that found its key in
//! row 500 of both a 20k-row and a 200k-row sheet would compare two identical
//! 500-cell scans and prove nothing about per-row cost.
//!
//! The method is the one `criteria_alloc.rs` established: a wrapping global
//! allocator with a THREAD-LOCAL counter (cargo runs tests on parallel threads
//! sharing one allocator, so a global counter would tally other tests' work
//! into this measurement), armed only around the call under test, and compared
//! across a 10x change in row count. Identical counts at 10x the rows is a far
//! sharper instrument than any timing threshold.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use ferrix_core::{CellRef, ErrorKind, Sheet, Value};
use ferrix_formula::{eval, parse};

struct Counting;

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

/// Run `f` with counting on, returning the allocations made on THIS thread.
fn allocations_during<T>(f: impl FnOnce() -> T) -> (T, u64) {
    ALLOCS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, ALLOCS.with(|c| c.get()))
}

/// A sheet with `rows` rows:
///   A: ascending numeric keys `r * 10` — so any key not a multiple of 10, or
///      above the top, is guaranteed absent
///   B: one of three interned mixed-case strings, so text comparison has real
///      text to fold on every single probe
///   C: payload `r`
fn synthetic(rows: u32) -> Sheet {
    let mut s = Sheet::new("big");
    let regions = ["North", "South", "Northeast"];
    for r in 0..rows {
        s.set(CellRef::new(r, 0), Value::Number(r as f64 * 10.0));
        s.set_text(CellRef::new(r, 1), regions[(r % 3) as usize]);
        s.set(CellRef::new(r, 2), Value::Number(r as f64));
    }
    s
}

/// Evaluate `template` (with `{n}` replaced by the row count) over two sheets
/// 10x apart in height; require identical, small allocation counts and the
/// stated result on both.
#[track_caller]
fn assert_flat_allocations(template: &str, expected: Value, ceiling: u64) {
    const SMALL: u32 = 20_000;
    const LARGE: u32 = 200_000;

    let small = synthetic(SMALL);
    let large = synthetic(LARGE);

    let f_small = template.replace("{n}", &SMALL.to_string());
    let f_large = template.replace("{n}", &LARGE.to_string());
    let e_small = parse(&f_small).unwrap_or_else(|e| panic!("parse {f_small}: {e}"));
    let e_large = parse(&f_large).unwrap_or_else(|e| panic!("parse {f_large}: {e}"));

    // Warm up: the first eval may touch lazily-initialised state that is
    // legitimately one-off and not per-row.
    let _ = eval(&e_small, &small);
    let _ = eval(&e_large, &large);

    let (v_small, a_small) = allocations_during(|| eval(&e_small, &small));
    let (v_large, a_large) = allocations_during(|| eval(&e_large, &large));

    // Sanity: the lookup did the work we think it did. Without this the test
    // could pass by failing early and touching nothing.
    assert_eq!(
        v_small, expected,
        "{f_small} produced {v_small:?}; the measurement is only meaningful if \
         the lookup actually ran"
    );
    assert_eq!(v_large, expected, "{f_large} produced {v_large:?}");

    assert_eq!(
        a_small,
        a_large,
        "{template} allocated {a_small} times over {SMALL} rows but {a_large} \
         times over {LARGE} rows — {} extra allocations for {} extra rows, \
         i.e. the lookup allocates per cell and peak memory now scales with \
         row count instead of viewport size",
        a_large.saturating_sub(a_small),
        LARGE - SMALL
    );
    assert!(
        a_large <= ceiling,
        "{template} made {a_large} allocations over {LARGE} rows; expected at \
         most {ceiling} — a handful of setup allocations bounded by the \
         argument count, not by the data"
    );
}

const NA: Value = Value::Error(ErrorKind::NotAvailable);

#[test]
fn exact_vlookup_scanning_the_whole_column_does_not_allocate_per_row() {
    // Key 7 is not a multiple of 10, so it is absent and the exact scan walks
    // every row of both sheets — 20,000 cells vs 200,000.
    assert_flat_allocations("=VLOOKUP(7,A1:C{n},3,FALSE)", NA, 32);
}

#[test]
fn approximate_vlookup_does_not_allocate_per_probe() {
    // Binary search: ~15 probes vs ~18. Any per-probe allocation shows up.
    assert_flat_allocations("=VLOOKUP(105,A1:C{n},3,TRUE)", Value::Number(10.0), 32);
}

#[test]
fn match_does_not_allocate_per_row_in_any_match_type() {
    // Type 0 over an absent key: full scan.
    assert_flat_allocations("=MATCH(7,A1:A{n},0)", NA, 32);
    // Type 1: binary.
    assert_flat_allocations("=MATCH(105,A1:A{n},1)", Value::Number(11.0), 32);
    // Type -1 over ASCENDING data is a mis-sorted query, which is exactly why
    // it is worth measuring: the probes still have to stay allocation-free.
    // A key above the top makes the answer #N/A on both sheets, so the
    // measurement compares equal work rather than a row-count-dependent hit.
    assert_flat_allocations("=MATCH(99999999,A1:A{n},-1)", NA, 32);
}

#[test]
fn text_lookup_does_not_allocate_per_row_to_case_fold() {
    // The classic regression: folding a cell for comparison with
    // `to_lowercase()` allocates a String per row. The probe is upper-case and
    // absent, so every row is folded and compared and none of them match.
    assert_flat_allocations("=MATCH(\"NOWHERE\",B1:B{n},0)", NA, 32);
}

#[test]
fn wildcard_lookup_compiles_its_pattern_once_not_once_per_row() {
    // A wildcard probe must compile ONE `Pattern` per call; compiling it
    // inside the scan loop would allocate several times per row. The pattern
    // matches nothing, so the scan runs to the end.
    assert_flat_allocations("=MATCH(\"Nowhere*\",B1:B{n},0)", NA, 32);
    assert_flat_allocations("=XLOOKUP(\"Nowhere?\",B1:B{n},C1:C{n},NA(),2)", NA, 32);
}

#[test]
fn xlookup_does_not_allocate_per_row_in_any_search_mode() {
    // Forward linear, absent key: full scan.
    assert_flat_allocations("=XLOOKUP(7,A1:A{n},C1:C{n})", NA, 32);
    // Reverse linear, absent key: full scan from the other end.
    assert_flat_allocations("=XLOOKUP(7,A1:A{n},C1:C{n},NA(),0,-1)", NA, 32);
    // Binary ascending.
    assert_flat_allocations(
        "=XLOOKUP(100,A1:A{n},C1:C{n},NA(),0,2)",
        Value::Number(10.0),
        32,
    );
    // Nearest-larger with a LINEAR search mode: the one path in this module
    // that must visit every cell by definition. It still must not allocate
    // for any of them — that is what keeps its memory O(1) even though its
    // visit count is O(n).
    assert_flat_allocations(
        "=XLOOKUP(105,A1:A{n},C1:C{n},NA(),1)",
        Value::Number(11.0),
        32,
    );
    // Nearest-smaller, likewise.
    assert_flat_allocations(
        "=XLOOKUP(105,A1:A{n},C1:C{n},NA(),-1)",
        Value::Number(10.0),
        32,
    );
}

#[test]
fn index_does_not_allocate_for_a_direct_address() {
    assert_flat_allocations("=INDEX(A1:C{n},5000,3)", Value::Number(4999.0), 16);
}

#[test]
fn indirect_does_not_allocate_more_as_the_sheet_grows() {
    // INDIRECT parses its ref_text on every evaluate — it must, because a
    // cached target would be silently stale (see the module docs). That parse
    // is a fixed cost per call and must not grow with the sheet.
    assert_flat_allocations("=INDIRECT(\"C5000\")", Value::Number(4999.0), 32);
}

#[test]
fn a_composed_index_match_does_not_allocate_per_row() {
    // The shape real workbooks actually write. Both halves stream, so the
    // composition must too. "Northeast" sits at row 3 (r % 3 == 2), so this
    // one is an early exit — included to prove the composition itself adds no
    // allocation, with the full-scan cases above covering scan cost.
    assert_flat_allocations(
        "=INDEX(C1:C{n},MATCH(\"Northeast\",B1:B{n},0))",
        Value::Number(2.0),
        32,
    );
}
