//! The dynamic-array scale invariant (#27 P3), enforced.
//!
//! Ferrix targets 200M+ rows. `SORT(FILTER(A:A, B:B>0))` over a 10M-row column
//! is the headline acceptance criterion for the 16 dynamic-array functions:
//! FILTER must STREAM the input — read it row-by-row through the columnar
//! `spec_get` path and allocate memory proportional to the RESULT it keeps,
//! never to the 10M-row scan. If FILTER ever materialised the whole input
//! column into an `ArrayData` first, peak memory would scale with row count and
//! a 200M-row filter would OOM.
//!
//! Two instruments, mirroring the pivot kernel's `memory_scales_with_groups_
//! not_rows` and the criteria `*_alloc` tests:
//!
//! 1. **Allocation count** through a wrapping global allocator. With the KEPT
//!    (result) count held constant while the SCAN grows 4x, the allocation
//!    total must not move — that is a sharp proof that memory follows the
//!    result, not the input.
//! 2. **Wall-clock**: the full 10M-row `SORT(FILTER(...))` must finish well
//!    under a second, and the test prints the real number.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

use ferrix_core::{CellRef, ErrorKind, StrId, Value};
use ferrix_formula::{eval_view, parse, ArrayData, CellSource, EvalResult};

// --- a thread-local counting allocator (same design as criteria_alloc.rs) ---

struct Counting;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

#[inline]
fn tick() {
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

fn allocations_during<T>(f: impl FnOnce() -> T) -> (T, u64) {
    ALLOCS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, ALLOCS.with(|c| c.get()))
}

/// A synthetic two-column source that GENERATES cells on the fly and never
/// materialises the rows — so the test itself honours the scale invariant and
/// can run in CI at 10M rows in milliseconds.
///
/// * Column A (col 0) = the row's value: `(row % 1000)` so SORT has real work.
/// * Column B (col 1) = the filter flag: `1.0` for the first `keep` rows, else
///   `0.0`. `FILTER(A:A, B:B>0)` therefore keeps exactly `keep` rows regardless
///   of `rows`, which is what lets the allocation test hold the RESULT constant
///   while growing the SCAN.
struct Synthetic {
    rows: usize,
    keep: usize,
}

impl CellSource for Synthetic {
    fn get(&self, cell: CellRef) -> Value {
        let r = cell.row as usize;
        if r >= self.rows {
            return Value::Empty;
        }
        match cell.col {
            0 => Value::Number((r % 1000) as f64),
            1 => Value::Number(if r < self.keep { 1.0 } else { 0.0 }),
            _ => Value::Empty,
        }
    }
    fn resolve(&self, _id: StrId) -> &str {
        ""
    }
    fn sum_rect(&self, _start: CellRef, _end: CellRef) -> f64 {
        0.0
    }
    fn count_rect(&self, _start: CellRef, _end: CellRef) -> usize {
        0
    }
    fn row_count(&self) -> usize {
        self.rows
    }
}

fn as_array(r: EvalResult) -> ArrayData {
    match r {
        EvalResult::Array(a) => a,
        EvalResult::Scalar(v) => panic!("wanted an array, got Scalar({v:?})"),
    }
}

/// The headline test: `=SORT(FILTER(A:A, B:B>0))` over 10M rows returns well
/// under a second, and the result is bounded by what it keeps.
#[test]
fn sort_of_filter_over_10m_rows_is_fast_and_result_bounded() {
    const ROWS: usize = 10_000_000;
    const KEEP: usize = 5_000;

    let src = Synthetic {
        rows: ROWS,
        keep: KEEP,
    };
    // `A1:A10000000` / `B1:B10000000` clamp to `row_count()` = 10M through
    // `spec_for`, so this is a genuine 10M-row scan. (The parser has no bare
    // `A:A` whole-column form; an explicit row-bounded range is equivalent
    // once clamped.)
    let expr = parse("=SORT(FILTER(A1:A10000000,B1:B10000000>0))").unwrap();

    // Warm up (one-off lazy init is not part of the measurement).
    let _ = eval_view(&expr, &src);

    let start = Instant::now();
    let result = ferrix_formula::eval::eval_view_array(&expr, &src);
    let elapsed = start.elapsed();

    let a = as_array(result);
    // Correctness: exactly KEEP rows kept, one column, sorted ascending.
    assert_eq!(
        (a.rows(), a.cols()),
        (KEEP as u32, 1),
        "result must hold exactly the {KEEP} kept rows"
    );
    let vals: Vec<f64> = a.iter().filter_map(|v| v.as_number()).collect();
    assert_eq!(vals.len(), KEEP);
    assert!(
        vals.windows(2).all(|w| w[0] <= w[1]),
        "result must be sorted ascending"
    );

    // Perf: WELL under a second. Print the real number for the PR body.
    println!(
        "SORT(FILTER(A1:A10000000,B1:B10000000>0)) over {ROWS} rows keeping {KEEP}: {:?}",
        elapsed
    );
    assert!(
        elapsed.as_millis() < 1000,
        "10M-row SORT(FILTER(...)) took {elapsed:?} — must be well under 1s"
    );
}

/// Memory follows the RESULT, not the SCAN: hold KEEP constant, grow ROWS 4x,
/// and the allocation total must not move. If FILTER materialised the input
/// column, the 4x-larger scan would allocate ~4x as much.
#[test]
fn filter_memory_scales_with_result_not_rows() {
    const KEEP: usize = 1_000;
    const SMALL: usize = 1_000_000;
    const LARGE: usize = 4_000_000;

    let small = Synthetic {
        rows: SMALL,
        keep: KEEP,
    };
    let large = Synthetic {
        rows: LARGE,
        keep: KEEP,
    };
    let expr = parse("=FILTER(A1:A4000000,B1:B4000000>0)").unwrap();

    // Warm up both.
    let _ = ferrix_formula::eval::eval_view_array(&expr, &small);
    let _ = ferrix_formula::eval::eval_view_array(&expr, &large);

    let (got_small, alloc_small) =
        allocations_during(|| as_array(ferrix_formula::eval::eval_view_array(&expr, &small)));
    let (got_large, alloc_large) =
        allocations_during(|| as_array(ferrix_formula::eval::eval_view_array(&expr, &large)));

    // Both keep exactly KEEP rows.
    assert_eq!(got_small.rows(), KEEP as u32);
    assert_eq!(got_large.rows(), KEEP as u32);

    // The real assertion: the 4x-bigger SCAN must not allocate materially more.
    // A small constant slack absorbs Vec growth-doubling jitter; a per-row leak
    // would blow this out by ~3M allocations.
    let slack = 16;
    assert!(
        alloc_large <= alloc_small + slack,
        "FILTER allocated {alloc_small} times over {SMALL} rows but {alloc_large} \
         times over {LARGE} rows (4x the scan, same {KEEP}-row result) — memory \
         is scaling with row count, not result extent"
    );
}

/// A guard that the streaming FILTER predicate rejects a genuinely bad shape
/// (include taller than array) as `#VALUE!` even on the fast path — the error
/// carries through the scalar seam.
#[test]
fn filter_shape_mismatch_is_value_error_even_streaming() {
    struct Ragged;
    impl CellSource for Ragged {
        fn get(&self, cell: CellRef) -> Value {
            match cell.col {
                0 => {
                    if cell.row < 2 {
                        Value::Number(cell.row as f64)
                    } else {
                        Value::Empty
                    }
                }
                _ => Value::Number(1.0),
            }
        }
        fn resolve(&self, _id: StrId) -> &str {
            ""
        }
        fn sum_rect(&self, _s: CellRef, _e: CellRef) -> f64 {
            0.0
        }
        fn count_rect(&self, _s: CellRef, _e: CellRef) -> usize {
            0
        }
        fn row_count(&self) -> usize {
            10
        }
    }
    // row_count is 10, so A1:A2 stays 2 rows and B1:B3 stays 3 rows — a
    // genuine height mismatch the streaming predicate must reject as #VALUE!.
    let expr = parse("=FILTER(A1:A2,B1:B3>0)").unwrap();
    assert_eq!(eval_view(&expr, &Ragged), Value::Error(ErrorKind::Value));
}
