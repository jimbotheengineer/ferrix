//! Unit tests for the text functions.
//!
//! Deliberately in their own file so two sibling agents adding date and
//! statistical functions to the same evaluator cannot conflict with them.
//!
//! Every assertion below is written to fail against dead code: results are
//! compared to exact expected strings/numbers, never to "non-empty" or "not an
//! error". A function that returned its input unchanged, or `#VALUE!`, fails
//! these.

use ferrix_core::{CellRef, ErrorKind, Sheet, Value};

use crate::{eval, parse};

/// Evaluate a formula against a sheet and render the result as a string, so a
/// test can assert on the exact text a user would see.
fn t(sheet: &Sheet, f: &str) -> String {
    let expr = parse(f).unwrap_or_else(|e| panic!("parse {f}: {e}"));
    match eval(&expr, sheet) {
        Value::Text(id) => sheet.resolve(id).to_string(),
        Value::Number(n) => ferrix_core::format_number(n),
        Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Empty => String::new(),
        Value::Error(e) => e.to_string(),
    }
}

fn v(sheet: &Sheet, f: &str) -> Value {
    eval(&parse(f).unwrap(), sheet)
}

/// A1 = "  Hello   World  ", A2 = "café", A3 = 42, A4 = "a-b-c-d", A5 = "".
fn fixture() -> Sheet {
    let mut s = Sheet::new("t");
    s.set_text(CellRef::new(0, 0), "  Hello   World  ");
    s.set_text(CellRef::new(1, 0), "café");
    s.set(CellRef::new(2, 0), Value::Number(42.0));
    s.set_text(CellRef::new(3, 0), "a-b-c-d");
    s.set_text(CellRef::new(4, 0), "");
    s.set_text(CellRef::new(5, 0), "naïve Ünicode ĝarden");
    s
}

// --- length and case ------------------------------------------------------

#[test]
fn len_counts_characters_not_bytes() {
    let s = fixture();
    // "café" is 5 BYTES and 4 CHARACTERS. A byte-oriented LEN returns 5.
    assert_eq!(t(&s, "=LEN(A2)"), "4");
    assert_eq!(t(&s, r#"=LEN("日本語")"#), "3");
    assert_eq!(t(&s, "=LEN(A1)"), "17");
    assert_eq!(t(&s, "=LEN(A5)"), "0");
    // Numbers coerce to their displayed text.
    assert_eq!(t(&s, "=LEN(A3)"), "2");
}

#[test]
fn case_functions_handle_non_ascii() {
    let s = fixture();
    assert_eq!(t(&s, "=UPPER(A2)"), "CAFÉ");
    assert_eq!(t(&s, "=LOWER(A6)"), "naïve ünicode ĝarden");
    // An ASCII-only implementation leaves these unchanged, which is the bug
    // this pins.
    assert_ne!(t(&s, "=UPPER(A2)"), "CAFé");
    assert_eq!(t(&s, r#"=UPPER("straße")"#), "STRASSE");
}

#[test]
fn proper_capitalises_each_word() {
    let s = fixture();
    assert_eq!(t(&s, r#"=PROPER("hello wORLD")"#), "Hello World");
    assert_eq!(t(&s, r#"=PROPER("o'neil-smith")"#), "O'Neil-Smith");
    assert_eq!(t(&s, r#"=PROPER("2nd place")"#), "2Nd Place");
}

#[test]
fn trim_collapses_internal_runs_and_keeps_tabs() {
    let s = fixture();
    assert_eq!(t(&s, "=TRIM(A1)"), "Hello World");
    assert_eq!(t(&s, r#"=TRIM("   ")"#), "");
    // Excel TRIM touches only U+0020. A `str::trim`-based version eats \t.
    assert_eq!(t(&s, "=TRIM(\"a\tb\")"), "a\tb");
}

#[test]
fn clean_strips_control_characters_only() {
    let s = fixture();
    assert_eq!(t(&s, "=CLEAN(\"a\u{7}b\u{1}c\")"), "abc");
    assert_eq!(t(&s, "=CLEAN(\"a\tb\nc\")"), "abc");
    // Non-ASCII is not a control character and must survive.
    assert_eq!(t(&s, "=CLEAN(A2)"), "café");
}

// --- substring extraction -------------------------------------------------

#[test]
fn left_right_mid_are_char_oriented() {
    let s = fixture();
    // Byte slicing "café" at 3 would split the é and panic or produce mojibake.
    assert_eq!(t(&s, "=LEFT(A2,3)"), "caf");
    assert_eq!(t(&s, "=LEFT(A2,4)"), "café");
    assert_eq!(t(&s, "=RIGHT(A2,2)"), "fé");
    assert_eq!(t(&s, "=MID(A2,4,1)"), "é");
    assert_eq!(t(&s, r#"=MID("日本語です",2,2)"#), "本語");
    // Defaults and over-long counts clamp rather than error.
    assert_eq!(t(&s, "=LEFT(A2)"), "c");
    assert_eq!(t(&s, "=RIGHT(A2)"), "é");
    assert_eq!(t(&s, "=LEFT(A2,99)"), "café");
    assert_eq!(t(&s, "=MID(A2,3,99)"), "fé");
}

#[test]
fn mid_start_past_end_is_empty_and_zero_start_is_error() {
    let s = fixture();
    // Excel: start past the end yields "", start < 1 is #VALUE!.
    assert_eq!(t(&s, "=MID(A2,10,3)"), "");
    assert_eq!(v(&s, "=MID(A2,0,3)"), Value::Error(ErrorKind::Value));
    assert_eq!(v(&s, "=LEFT(A2,-1)"), Value::Error(ErrorKind::Value));
}

#[test]
fn rept_repeats_and_refuses_to_explode() {
    let s = fixture();
    assert_eq!(t(&s, r#"=REPT("ab",3)"#), "ababab");
    assert_eq!(t(&s, r#"=REPT("ab",0)"#), "");
    // 1e9 copies must be an error, not an allocation.
    assert_eq!(
        v(&s, r#"=REPT("ab",1000000000)"#),
        Value::Error(ErrorKind::Value)
    );
}

// --- FIND / SEARCH --------------------------------------------------------

#[test]
fn find_is_case_sensitive_search_is_not() {
    let s = fixture();
    // The core distinction. If FIND folded case, the first line would be 1.
    assert_eq!(
        v(&s, r#"=FIND("h","Hello")"#),
        Value::Error(ErrorKind::Value)
    );
    assert_eq!(t(&s, r#"=FIND("H","Hello")"#), "1");
    assert_eq!(t(&s, r#"=SEARCH("h","Hello")"#), "1");
    assert_eq!(t(&s, r#"=SEARCH("LL","Hello")"#), "3");
}

#[test]
fn search_accepts_wildcards_and_find_does_not() {
    let s = fixture();
    // `?` = one char, `*` = any run — the same syntax COUNTIF uses, because it
    // is literally the same compiled Pattern.
    assert_eq!(t(&s, r#"=SEARCH("b?d","abcde")"#), "2");
    assert_eq!(t(&s, r#"=SEARCH("a*e","xxabcde")"#), "3");
    assert_eq!(t(&s, r#"=SEARCH("*","abc")"#), "1");
    // FIND treats them literally, so a wildcard needle simply is not found.
    assert_eq!(
        v(&s, r#"=FIND("b?d","abcde")"#),
        Value::Error(ErrorKind::Value)
    );
    assert_eq!(t(&s, r#"=FIND("b?d","ab?de")"#), "2");
    // `~` escapes, inherited from the criteria matcher.
    assert_eq!(t(&s, r#"=SEARCH("~*","a*b")"#), "2");
}

#[test]
fn find_and_search_are_one_based_and_char_oriented() {
    let s = fixture();
    // "café X": the X is character 6, but byte 7.
    assert_eq!(t(&s, r#"=FIND("X","café X")"#), "6");
    assert_eq!(t(&s, r#"=SEARCH("x","café X")"#), "6");
    assert_eq!(t(&s, r#"=FIND("é","café")"#), "4");
}

#[test]
fn start_position_past_the_end_is_value_error() {
    let s = fixture();
    // The acceptance criterion, spelled out: a position past the end is
    // #VALUE!, not a clamp and not an empty result.
    assert_eq!(
        v(&s, r#"=FIND("a","abc",5)"#),
        Value::Error(ErrorKind::Value)
    );
    assert_eq!(
        v(&s, r#"=SEARCH("a","abc",5)"#),
        Value::Error(ErrorKind::Value)
    );
    assert_eq!(
        v(&s, r#"=FIND("a","abc",0)"#),
        Value::Error(ErrorKind::Value)
    );
    // Start inside the string skips earlier hits and still reports an absolute
    // 1-based position.
    assert_eq!(t(&s, r#"=FIND("a","banana",3)"#), "4");
    assert_eq!(t(&s, r#"=SEARCH("A","banana",5)"#), "6");
    // Not present at all is also #VALUE!.
    assert_eq!(v(&s, r#"=FIND("z","abc")"#), Value::Error(ErrorKind::Value));
}

// --- SUBSTITUTE / REPLACE -------------------------------------------------

#[test]
fn substitute_replaces_all_or_one_instance() {
    let s = fixture();
    assert_eq!(t(&s, "=SUBSTITUTE(A4,\"-\",\"+\")"), "a+b+c+d");
    assert_eq!(t(&s, "=SUBSTITUTE(A4,\"-\",\"+\",2)"), "a-b+c-d");
    assert_eq!(t(&s, "=SUBSTITUTE(A4,\"-\",\"\")"), "abcd");
    // Case-sensitive, like Excel: "A" does not match "a".
    assert_eq!(t(&s, r#"=SUBSTITUTE("banana","A","X")"#), "banana");
    assert_eq!(t(&s, r#"=SUBSTITUTE("banana","a","X")"#), "bXnXnX");
    // An instance number past the number of occurrences changes nothing.
    assert_eq!(t(&s, "=SUBSTITUTE(A4,\"-\",\"+\",9)"), "a-b-c-d");
    // Multi-byte old/new must not corrupt the string.
    assert_eq!(t(&s, r#"=SUBSTITUTE("café","é","e")"#), "cafe");
}

#[test]
fn replace_uses_one_based_char_positions() {
    let s = fixture();
    assert_eq!(t(&s, r#"=REPLACE("abcdef",2,3,"XY")"#), "aXYef");
    assert_eq!(t(&s, r#"=REPLACE("abcdef",1,0,"Z")"#), "Zabcdef");
    // Character positions, not byte positions: replacing char 4 of "café"
    // must take the é, and a byte-based version would split it.
    assert_eq!(t(&s, r#"=REPLACE("café",4,1,"e")"#), "cafe");
    assert_eq!(
        v(&s, r#"=REPLACE("abc",0,1,"Z")"#),
        Value::Error(ErrorKind::Value)
    );
}

// --- joining --------------------------------------------------------------

#[test]
fn concat_joins_scalars_and_ranges() {
    let mut s = Sheet::new("j");
    s.set_text(CellRef::new(0, 0), "a");
    s.set_text(CellRef::new(1, 0), "b");
    s.set_text(CellRef::new(2, 0), "c");
    assert_eq!(t(&s, r#"=CONCAT("x","y",1)"#), "xy1");
    assert_eq!(t(&s, "=CONCAT(A1:A3)"), "abc");
    assert_eq!(t(&s, "=CONCATENATE(A1,A2,A3)"), "abc");
    // CONCATENATE is the scalar-only form; a range there is an error.
    assert_eq!(v(&s, "=CONCATENATE(A1:A3)"), Value::Error(ErrorKind::Value));
}

#[test]
fn textjoin_honours_ignore_empty() {
    let mut s = Sheet::new("j");
    s.set_text(CellRef::new(0, 0), "a");
    s.set_text(CellRef::new(1, 0), "");
    s.set_text(CellRef::new(2, 0), "c");
    // The whole acceptance criterion in two lines: the SAME range, the same
    // delimiter, and only the flag differs.
    assert_eq!(t(&s, r#"=TEXTJOIN("-",TRUE,A1:A3)"#), "a-c");
    assert_eq!(t(&s, r#"=TEXTJOIN("-",FALSE,A1:A3)"#), "a--c");
    assert_eq!(t(&s, r#"=TEXTJOIN(", ",TRUE,"x","","y")"#), "x, y");
    assert_eq!(t(&s, r#"=TEXTJOIN("",TRUE,A1:A3)"#), "ac");
}

// --- TEXT / VALUE ---------------------------------------------------------

#[test]
fn text_routes_through_the_numfmt_engine() {
    let s = fixture();
    // These are the numfmt engine's outputs, not a private formatter's: if
    // TEXT grew its own parser it would have to reimplement grouping,
    // padding, negative sections and the text slot, and would drift.
    assert_eq!(t(&s, r##"=TEXT(1234.5,"#,##0.00")"##), "1,234.50");
    assert_eq!(t(&s, r#"=TEXT(0.5,"0%")"#), "50%");
    assert_eq!(t(&s, r#"=TEXT(-5,"0.00;(0.00)")"#), "(5.00)");
    assert_eq!(t(&s, r#"=TEXT(7,"000")"#), "007");
    // Cross-check: the same code through NumFmt directly must agree.
    let nf = ferrix_core::numfmt::NumFmt::parse("#,##0.00");
    assert_eq!(t(&s, r##"=TEXT(1234.5,"#,##0.00")"##), nf.render(1234.5));
}

#[test]
fn value_parses_spreadsheet_decorations() {
    let s = fixture();
    assert_eq!(t(&s, r#"=VALUE("123")"#), "123");
    assert_eq!(t(&s, r#"=VALUE("  -12.5 ")"#), "-12.5");
    assert_eq!(t(&s, r#"=VALUE("1,234")"#), "1234");
    assert_eq!(t(&s, r#"=VALUE("$1,234.50")"#), "1234.5");
    assert_eq!(t(&s, r#"=VALUE("50%")"#), "0.5");
    assert_eq!(t(&s, r#"=VALUE("(25)")"#), "-25");
    assert_eq!(v(&s, r#"=VALUE("abc")"#), Value::Error(ErrorKind::Value));
    assert_eq!(v(&s, r#"=VALUE("")"#), Value::Error(ErrorKind::Value));
    // Round trip through TEXT.
    assert_eq!(t(&s, r##"=VALUE(TEXT(1234.5,"#,##0.00"))"##), "1234.5");
}

// --- errors and composition ----------------------------------------------

#[test]
fn errors_propagate_through_text_functions() {
    let mut s = Sheet::new("e");
    s.set(CellRef::new(0, 0), Value::Error(ErrorKind::DivZero));
    assert_eq!(v(&s, "=LEN(A1)"), Value::Error(ErrorKind::DivZero));
    assert_eq!(v(&s, "=UPPER(A1)"), Value::Error(ErrorKind::DivZero));
    assert_eq!(v(&s, "=LEFT(A1,2)"), Value::Error(ErrorKind::DivZero));
    // Wrong arity is #VALUE!, not a panic or a silent default.
    assert_eq!(v(&s, "=LEN()"), Value::Error(ErrorKind::Value));
    assert_eq!(v(&s, "=MID(\"abc\",1)"), Value::Error(ErrorKind::Value));
}

#[test]
fn text_functions_nest() {
    let s = fixture();
    assert_eq!(t(&s, "=UPPER(TRIM(A1))"), "HELLO WORLD");
    assert_eq!(t(&s, "=LEN(TRIM(A1))"), "11");
    assert_eq!(t(&s, r#"=MID(A1,FIND("W",A1),5)"#), "World");
    assert_eq!(t(&s, "=LEFT(UPPER(A2),3)"), "CAF");
}

#[test]
fn unknown_function_still_reports_name_error() {
    // Guards the delegating arm in eval.rs: it must claim ONLY text names.
    let s = fixture();
    assert_eq!(v(&s, "=LEFTISH(A1,2)"), Value::Error(ErrorKind::Name));
    assert!(!crate::text::is_text_fn("SUM"));
    assert!(!crate::text::is_text_fn("LEFTISH"));
    assert!(crate::text::is_text_fn("LEFT"));
}

// --- scale ----------------------------------------------------------------

#[test]
fn len_on_a_200m_row_shaped_column_is_o1_per_cell() {
    // The acceptance criterion: LEN must cost the length of ONE cell,
    // regardless of how tall the column it sits in is.
    //
    // A row count is not materialisable at 200M in a test, so the shape is
    // faked the way the engine would actually see it: a source that REPORTS
    // 200M rows and answers any cell in O(1), and that COUNTS how many cells
    // each evaluation touches. An implementation that walked the column — or
    // that consulted row_count at all in a way that scaled — shows up as a
    // touch count that grows.
    use std::cell::Cell;

    use ferrix_core::{StrId, StringArena};

    struct TallColumn {
        arena: StringArena,
        id: StrId,
        touches: Cell<usize>,
        rows: usize,
    }

    impl crate::CellSource for TallColumn {
        fn get(&self, _cell: CellRef) -> Value {
            self.touches.set(self.touches.get() + 1);
            Value::Text(self.id)
        }
        fn resolve(&self, id: StrId) -> &str {
            self.arena.resolve(id).unwrap_or("")
        }
        fn sum_rect(&self, _s: CellRef, _e: CellRef) -> f64 {
            0.0
        }
        fn count_rect(&self, _s: CellRef, _e: CellRef) -> usize {
            0
        }
        fn row_count(&self) -> usize {
            self.rows
        }
    }

    let mut arena = StringArena::new();
    let id = arena.intern("Ferrix");
    let short = TallColumn {
        arena: arena.clone(),
        id,
        touches: Cell::new(0),
        rows: 1_000,
    };
    let tall = TallColumn {
        arena,
        id,
        touches: Cell::new(0),
        rows: 200_000_000,
    };

    let expr = parse("=LEN(A500)").unwrap();

    let t0 = std::time::Instant::now();
    assert_eq!(crate::eval_view(&expr, &short), Value::Number(6.0));
    let short_touches = short.touches.get();

    assert_eq!(crate::eval_view(&expr, &tall), Value::Number(6.0));
    let tall_touches = tall.touches.get();
    let elapsed = t0.elapsed();

    assert_eq!(
        short_touches, 1,
        "LEN read {short_touches} cells to answer about one cell"
    );
    assert_eq!(
        tall_touches, short_touches,
        "LEN touched {short_touches} cells in a 1,000-row column but \
         {tall_touches} in a 200,000,000-row one — the cost is scaling with \
         the column height, so a 200M-row LEN column is O(rows) per cell"
    );
    // A walk of even a tiny fraction of 200M rows could not finish this fast.
    assert!(
        elapsed.as_millis() < 500,
        "two LEN evaluations took {elapsed:?}; something is scanning"
    );
}

#[test]
fn interning_dedups_so_a_text_column_is_bounded_by_distinct_results() {
    // The interning criterion, measured rather than asserted by inspection.
    //
    // Counting the interner's global size would be flaky: cargo runs tests
    // concurrently and every other test in this file interns into the same
    // process-wide store. So measure the property directly instead — evaluate
    // the same text formula down 3,000 rows holding 3 distinct inputs and
    // count the DISTINCT `StrId`s handed back. An implementation that
    // allocated a fresh string per cell returns 3,000 different ids; a
    // deduplicating one returns 3.
    use std::collections::HashSet;

    use ferrix_core::arena::intern_formula_text;
    use ferrix_core::StrId;

    let mut s = Sheet::new("d");
    let regions = ["north", "south", "east"];
    for r in 0..3_000u32 {
        s.set_text(CellRef::new(r, 0), regions[(r % 3) as usize]);
    }

    let mut ids: HashSet<StrId> = HashSet::new();
    for r in 1..=3_000u32 {
        match v(&s, &format!("=UPPER(A{r})")) {
            Value::Text(id) => {
                let want = regions[((r - 1) % 3) as usize].to_uppercase();
                assert_eq!(s.resolve(id), want, "row {r}");
                ids.insert(id);
            }
            other => panic!("row {r}: expected text, got {other:?}"),
        }
    }

    assert_eq!(
        ids.len(),
        3,
        "3,000 UPPER() cells over 3 distinct inputs produced {} distinct          string ids; results must dedup through the interner, or a 1M-row          text column costs 1M strings instead of 3",
        ids.len()
    );

    // Idempotence is what buys that: the same text always maps to the same id.
    assert_eq!(
        intern_formula_text("NORTH"),
        intern_formula_text("NORTH"),
        "interning the same string twice produced different ids"
    );
    assert_ne!(intern_formula_text("NORTH"), intern_formula_text("SOUTH"));
}

#[test]
fn concat_over_a_huge_range_stays_bounded() {
    // CONCAT of a range must cap rather than materialise the range. The
    // source below claims 200M rows; a non-streaming implementation would
    // either hang or exhaust memory instead of returning #VALUE!.
    use ferrix_core::{StrId, StringArena};

    struct Tall {
        arena: StringArena,
        id: StrId,
    }
    impl crate::CellSource for Tall {
        fn get(&self, _c: CellRef) -> Value {
            Value::Text(self.id)
        }
        fn resolve(&self, id: StrId) -> &str {
            self.arena.resolve(id).unwrap_or("")
        }
        fn sum_rect(&self, _s: CellRef, _e: CellRef) -> f64 {
            0.0
        }
        fn count_rect(&self, _s: CellRef, _e: CellRef) -> usize {
            0
        }
        fn row_count(&self) -> usize {
            200_000_000
        }
    }
    let mut arena = StringArena::new();
    let id = arena.intern("xxxxxxxxxx");
    let src = Tall { arena, id };

    let expr = parse("=CONCAT(A1:A200000000)").unwrap();
    let t0 = std::time::Instant::now();
    let got = crate::eval_view(&expr, &src);
    assert_eq!(
        got,
        Value::Error(ErrorKind::Value),
        "CONCAT over 200M rows should hit the 32,767-char cap and report \
         #VALUE!, not build a result proportional to the row count"
    );
    assert!(
        t0.elapsed().as_secs() < 5,
        "CONCAT took {:?} — it is walking rows past the output cap",
        t0.elapsed()
    );
}
