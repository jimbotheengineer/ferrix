use super::*;
use crate::{Column, StringArena, Value};

/// Build a column of `rows` rows whose text values cycle through `vals`.
fn text_column(arena: &mut StringArena, rows: usize, vals: &[&str]) -> Column {
    let ids: Vec<_> = vals.iter().map(|v| arena.intern(v)).collect();
    let mut c = Column::with_capacity(rows);
    for r in 0..rows {
        c.push(Value::Text(ids[r % ids.len()]));
    }
    c
}

#[test]
fn distinct_values_are_the_columns_text_values() {
    let mut arena = StringArena::new();
    let col = text_column(&mut arena, 300, &["apple", "banana", "cherry"]);
    let d = DistinctValues::scan(&col, 0, ScanBudget::default());
    assert_eq!(d.len(), 3, "three distinct strings cycled 100 times each");
    let mut got: Vec<&str> = d.ids().iter().map(|i| arena.resolve_or_empty(*i)).collect();
    got.sort_unstable();
    assert_eq!(got, vec!["apple", "banana", "cherry"]);
}

/// THE bound. A column with far more rows than the budget must not be walked.
///
/// Asserts on `rows_examined`, which is incremented inside the scan loop, so
/// it fails if anyone replaces the window with a full pass. A 200M-row
/// `Column` cannot be allocated in a test, so the row count is faked by
/// checking the budget against a column that is merely much larger than the
/// limit, plus the explicit statement that the count is limit-bounded and not
/// rows-bounded.
#[test]
fn scan_never_walks_more_rows_than_the_budget() {
    let mut arena = StringArena::new();
    let rows = 250_000;
    let col = text_column(&mut arena, rows, &["a", "b", "c", "d"]);
    let budget = ScanBudget::with_limit(SCAN_LIMIT);
    let d = DistinctValues::scan(&col, 0, budget);
    assert_eq!(
        d.budget.rows_examined, SCAN_LIMIT,
        "the scan must stop at the budget, not at the end of a {rows}-row column"
    );
    assert!(
        d.budget.rows_examined < rows,
        "examined {} of {rows} rows — this is the invariant that keeps a \
         200M-row column from being walked",
        d.budget.rows_examined
    );
    assert!(
        d.budget.truncated,
        "a column longer than the budget is a sample"
    );
}

/// The bound expressed as the criterion states it: a nominally 200M-row column.
///
/// `Column::ensure_rows` materialises the rows, so this uses the ratio the
/// real case has — the scan's cost is a CONSTANT, so the ratio of examined to
/// total shrinks without limit as the column grows. Pinning the absolute
/// number is what makes that true rather than asserted.
#[test]
fn scan_cost_is_constant_in_the_row_count() {
    let mut arena = StringArena::new();
    let small = text_column(&mut arena, 30_000, &["x", "y"]);
    let large = text_column(&mut arena, 400_000, &["x", "y"]);
    let a = DistinctValues::scan(&small, 0, ScanBudget::default());
    let b = DistinctValues::scan(&large, 0, ScanBudget::default());
    assert_eq!(
        a.budget.rows_examined, b.budget.rows_examined,
        "a 13x longer column must cost exactly the same to scan"
    );
    assert_eq!(a.budget.rows_examined, SCAN_LIMIT);
}

#[test]
fn scan_of_a_short_column_stops_at_its_end() {
    let mut arena = StringArena::new();
    let col = text_column(&mut arena, 12, &["p", "q"]);
    let d = DistinctValues::scan(&col, 0, ScanBudget::default());
    assert_eq!(d.budget.rows_examined, 12, "never read past the column");
    assert!(!d.budget.truncated);
}

#[test]
fn scan_window_follows_the_cursor() {
    // Values only present far down the column are found when the cursor is
    // there, and NOT found when the cursor is at the top — proving the window
    // moves rather than always starting at row 0.
    let mut arena = StringArena::new();
    let common = arena.intern("common");
    let rare = arena.intern("deepvalue");
    let rows = 200_000;
    let mut col = Column::with_capacity(rows);
    for r in 0..rows {
        col.push(Value::Text(if r == 150_000 { rare } else { common }));
    }
    let top = DistinctValues::scan(&col, 0, ScanBudget::default());
    let near = DistinctValues::scan(&col, 150_000, ScanBudget::default());
    let has = |d: &DistinctValues| d.ids().iter().any(|i| *i == rare);
    assert!(!has(&top), "row 150k is outside a 20k window starting at 0");
    assert!(has(&near), "the window must follow the edited row");
}

#[test]
fn distinct_set_is_capped() {
    let mut arena = StringArena::new();
    let mut col = Column::with_capacity(MAX_DISTINCT + 500);
    for r in 0..(MAX_DISTINCT + 500) {
        let id = arena.intern(&format!("v{r}"));
        col.push(Value::Text(id));
    }
    let d = DistinctValues::scan(&col, 0, ScanBudget::default());
    assert_eq!(d.len(), MAX_DISTINCT, "capped at ~10k distinct values");
    assert!(d.budget.distinct_capped);
    assert!(
        d.heap_bytes() < 128 * 1024,
        "the distinct set has a hard memory ceiling; got {} bytes",
        d.heap_bytes()
    );
}

#[test]
fn numbers_are_not_suggested() {
    let mut col = Column::with_capacity(100);
    for r in 0..100 {
        col.push(Value::Number(r as f64));
    }
    let d = DistinctValues::scan(&col, 0, ScanBudget::default());
    assert!(
        d.is_empty(),
        "autocompleting a number is harmful, never do it"
    );
}

#[test]
fn prefix_matches_come_before_containment_matches() {
    let mut arena = StringArena::new();
    let col = text_column(&mut arena, 60, &["Canada", "Chad", "Vatican", "Chile"]);
    let d = DistinctValues::scan(&col, 0, ScanBudget::default());
    let s = Suggestions::rank(&d, &arena, "can");
    assert_eq!(
        s.items,
        vec!["Canada".to_string(), "Vatican".to_string()],
        "prefix match first, containment second"
    );
}

#[test]
fn suggestions_are_case_insensitive_and_skip_the_exact_prefix() {
    let mut arena = StringArena::new();
    let col = text_column(&mut arena, 30, &["Alpha", "alphabet", "beta"]);
    let d = DistinctValues::scan(&col, 0, ScanBudget::default());
    let s = Suggestions::rank(&d, &arena, "ALPHA");
    assert_eq!(
        s.items,
        vec!["alphabet".to_string()],
        "`Alpha` equals the typed text and is not worth offering; \
         `alphabet` is"
    );
}

#[test]
fn an_empty_prefix_suggests_nothing() {
    let mut arena = StringArena::new();
    let col = text_column(&mut arena, 30, &["one", "two"]);
    let d = DistinctValues::scan(&col, 0, ScanBudget::default());
    assert!(Suggestions::rank(&d, &arena, "").is_empty());
    assert!(Suggestions::rank(&d, &arena, "   ").is_empty());
}

#[test]
fn suggestions_are_capped() {
    let mut arena = StringArena::new();
    let vals: Vec<String> = (0..40).map(|i| format!("prefix{i:02}")).collect();
    let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
    let col = text_column(&mut arena, 400, &refs);
    let d = DistinctValues::scan(&col, 0, ScanBudget::default());
    let s = Suggestions::rank(&d, &arena, "prefix");
    assert_eq!(s.len(), MAX_SUGGESTIONS);
    assert!(s.truncated);
}

#[test]
fn a_list_rule_supplies_the_suggestions_directly() {
    let vals: Vec<String> = ["North", "South", "East", "West"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let all = Suggestions::from_list(&vals, "");
    assert_eq!(all.len(), 4, "an empty prefix opens the whole dropdown");
    let one = Suggestions::from_list(&vals, "sou");
    assert_eq!(one.items, vec!["South".to_string()]);
}
