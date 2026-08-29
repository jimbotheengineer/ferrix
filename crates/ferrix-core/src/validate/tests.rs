use super::*;
use crate::table::{CmpOp, ValidationRule};
use crate::{CellRef, TableRange};

fn r(fr: u32, fc: u32, lr: u32, lc: u32) -> TableRange {
    TableRange::new(fr, fc, lr, lc)
}

fn cell(row: u32, col: u32) -> CellRef {
    CellRef::new(row, col)
}

// ------------------------------------------------------------ scale ------

/// THE scale criterion: a rule over 200,000,000 rows is one small entry.
#[test]
fn rule_over_200m_rows_stores_one_entry() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::new(
        r(0, 1, 199_999_999, 1),
        ValueDomain::WholeNumber,
        ValidationRule::Between {
            min: 1.0,
            max: 100.0,
        },
    ))
    .expect("push");
    assert_eq!(v.len(), 1, "one rule, however many rows it covers");
    assert!(
        v.heap_bytes() < 1024,
        "a 200M-row rule must cost under a kilobyte; got {}",
        v.heap_bytes()
    );
    // And the store genuinely governs the far end of that range.
    assert!(v.rule_for(cell(199_999_998, 1)).is_some());
    assert!(v.rule_for(cell(0, 2)).is_none(), "column C is not covered");
}

#[test]
fn rules_are_capped() {
    let mut v = SheetValidation::new();
    for i in 0..MAX_RULES {
        assert!(
            v.push(RangeValidation::new(
                r(i as u32, 0, i as u32, 0),
                ValueDomain::Any,
                ValidationRule::None
            ))
            .is_some(),
            "rule {i} should fit"
        );
    }
    assert!(
        v.push(RangeValidation::new(
            r(0, 0, 0, 0),
            ValueDomain::Any,
            ValidationRule::None
        ))
        .is_none(),
        "past MAX_RULES the store refuses rather than growing without bound"
    );
}

#[test]
fn the_last_matching_rule_wins() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::list(r(0, 0, 100, 0), vec!["a".into()]))
        .unwrap();
    v.push(RangeValidation::list(r(0, 0, 10, 0), vec!["b".into()]))
        .unwrap();
    let (i, rule) = v.rule_for(cell(5, 0)).expect("covered");
    assert_eq!(i, 1, "later entries override earlier ones");
    assert_eq!(rule.list_values(), Some(&["b".to_string()][..]));
    // Outside the second rectangle the first one still applies.
    assert_eq!(v.rule_for(cell(50, 0)).map(|(i, _)| i), Some(0));
}

#[test]
fn clearing_drops_every_overlapping_entry() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::list(r(0, 0, 10, 0), vec!["a".into()]))
        .unwrap();
    v.push(RangeValidation::list(r(20, 0, 30, 0), vec!["b".into()]))
        .unwrap();
    assert_eq!(v.clear_overlapping(r(5, 0, 6, 0)), 1);
    assert_eq!(v.len(), 1);
    assert!(v.rule_for(cell(25, 0)).is_some(), "the far rule survives");
}

// ------------------------------------------------- the six rule types ------

fn c(text: &str) -> Candidate<'_> {
    Candidate::from_input(text)
}

#[test]
fn list_rule_rejects_a_value_not_in_the_list() {
    let rule = RangeValidation::list(r(0, 0, 9, 0), vec!["North".into(), "South".into()]);
    assert_eq!(rule.check(&c("North"), None), None);
    assert_eq!(
        rule.check(&c("north"), None),
        None,
        "list membership is case-insensitive, as in the table model"
    );
    assert_eq!(rule.check(&c("Up"), None), Some(Violation::NotInList));
}

#[test]
fn whole_number_rule_rejects_a_fraction_and_an_out_of_range_integer() {
    let rule = RangeValidation::new(
        r(0, 0, 9, 0),
        ValueDomain::WholeNumber,
        ValidationRule::Between {
            min: 1.0,
            max: 10.0,
        },
    );
    assert_eq!(rule.check(&c("5"), None), None);
    assert_eq!(
        rule.check(&c("5.5"), None),
        Some(Violation::NotWhole),
        "a fraction fails the DOMAIN even though the bounds admit it"
    );
    assert_eq!(
        rule.check(&c("11"), None),
        Some(Violation::OutOfRange {
            min: 1.0,
            max: 10.0
        })
    );
    assert_eq!(
        rule.check(&c("hello"), None),
        Some(Violation::WrongType(crate::table::ColumnType::Number))
    );
}

#[test]
fn decimal_rule_accepts_a_fraction() {
    let rule = RangeValidation::new(
        r(0, 0, 9, 0),
        ValueDomain::Decimal,
        ValidationRule::Compare {
            op: CmpOp::Gt,
            value: 0.0,
        },
    );
    assert_eq!(rule.check(&c("0.25"), None), None);
    assert_eq!(
        rule.check(&c("-1"), None),
        Some(Violation::FailsCompare {
            op: CmpOp::Gt,
            value: 0.0
        })
    );
}

#[test]
fn date_rule_rejects_a_non_serial_and_text() {
    let rule = RangeValidation::new(
        r(0, 0, 9, 0),
        ValueDomain::Date,
        ValidationRule::Between {
            min: 44_000.0,
            max: 45_000.0,
        },
    );
    assert_eq!(rule.check(&c("44500"), None), None);
    assert_eq!(
        rule.check(&c("Tuesday"), None),
        Some(Violation::NotADate),
        "text is not a date"
    );
    assert_eq!(
        rule.check(&c("-5"), None),
        Some(Violation::NotADate),
        "a serial below 1900-01-01 is not a date"
    );
    assert_eq!(
        rule.check(&c("40000"), None),
        Some(Violation::OutOfRange {
            min: 44_000.0,
            max: 45_000.0
        }),
        "a real date outside the bounds is a range failure, not a type one"
    );
}

#[test]
fn text_length_rule_bounds_the_character_count() {
    let rule = RangeValidation::new(
        r(0, 0, 9, 0),
        ValueDomain::TextLength,
        ValidationRule::TextLength { min: 2, max: 4 },
    );
    assert_eq!(rule.check(&c("abc"), None), None);
    assert_eq!(
        rule.check(&c("a"), None),
        Some(Violation::BadLength {
            min: 2,
            max: 4,
            got: 1
        })
    );
    assert_eq!(
        rule.check(&c("abcde"), None),
        Some(Violation::BadLength {
            min: 2,
            max: 4,
            got: 5
        })
    );
    // Counts CHARACTERS, not bytes.
    assert_eq!(rule.check(&c("héé"), None), None);
}

#[test]
fn custom_formula_rule_defers_to_the_supplied_answer() {
    let rule = RangeValidation::new(
        r(0, 0, 9, 0),
        ValueDomain::Custom,
        ValidationRule::CustomFormula("=MOD(A1,2)=0".into()),
    );
    assert_eq!(rule.custom_formula(), Some("=MOD(A1,2)=0"));
    assert_eq!(rule.check(&c("4"), Some(true)), None);
    assert_eq!(
        rule.check(&c("3"), Some(false)),
        Some(Violation::CustomFailed)
    );
    assert_eq!(
        rule.check(&c("3"), None),
        None,
        "an unevaluated custom rule condemns nothing — a rule that cannot be \
         run is the rule's problem, not the data's"
    );
}

// -------------------------------------------------- messages and styles ----

#[test]
fn the_custom_message_is_what_the_user_is_shown() {
    let rule = RangeValidation::list(r(0, 0, 9, 0), vec!["Yes".into()])
        .with_message("Pick Yes. It is the only option.");
    let v = rule.check(&c("No"), None).expect("fails");
    assert_eq!(rule.explain(&v), "Pick Yes. It is the only option.");
}

#[test]
fn without_a_custom_message_the_violation_explains_itself() {
    let rule = RangeValidation::new(
        r(0, 0, 9, 0),
        ValueDomain::WholeNumber,
        ValidationRule::Between { min: 1.0, max: 3.0 },
    );
    let v = rule.check(&c("9"), None).expect("fails");
    assert_eq!(rule.explain(&v), "must be between 1 and 3");
}

#[test]
fn a_blank_custom_message_falls_back_rather_than_showing_nothing() {
    let rule = RangeValidation::list(r(0, 0, 9, 0), vec!["Yes".into()]).with_message("   ");
    let v = rule.check(&c("No"), None).expect("fails");
    assert_eq!(
        rule.explain(&v),
        "not an allowed value",
        "an empty message would render as a dialog with no explanation"
    );
}

#[test]
fn stop_rejects_and_warning_allows() {
    assert!(ErrorStyle::Stop.rejects());
    assert!(!ErrorStyle::Warning.rejects());
    assert!(!ErrorStyle::Information.rejects());
}

#[test]
fn allow_empty_governs_the_blank_cell() {
    let strict = RangeValidation::list(r(0, 0, 9, 0), vec!["a".into()]).with_allow_empty(false);
    let lax = RangeValidation::list(r(0, 0, 9, 0), vec!["a".into()]);
    assert_eq!(strict.check(&c(""), None), Some(Violation::Empty));
    assert_eq!(lax.check(&c(""), None), None);
}

// ---------------------------------------------------------- the dropdown ---

#[test]
fn only_a_list_rule_offers_a_dropdown() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::list(
        r(0, 0, 9, 0),
        vec!["Red".into(), "Green".into()],
    ))
    .unwrap();
    v.push(RangeValidation::new(
        r(0, 1, 9, 1),
        ValueDomain::Decimal,
        ValidationRule::None,
    ))
    .unwrap();
    assert_eq!(
        v.dropdown_for(cell(3, 0)),
        Some(&["Red".to_string(), "Green".to_string()][..])
    );
    assert_eq!(
        v.dropdown_for(cell(3, 1)),
        None,
        "a decimal rule has no list"
    );
    assert_eq!(v.dropdown_for(cell(3, 5)), None, "no rule, no dropdown");
}

#[test]
fn a_list_rule_with_the_dropdown_off_still_validates() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::list(r(0, 0, 9, 0), vec!["Red".into()]).with_dropdown(false))
        .unwrap();
    assert_eq!(v.dropdown_for(cell(1, 0)), None);
    assert!(
        v.check_cell(cell(1, 0), &c("Blue"), None).is_some(),
        "hiding the dropdown must not disable the rule"
    );
}

// ------------------------------------------------------------- candidate ---

#[test]
fn candidate_from_input_reads_the_same_shapes_the_editor_stores() {
    assert_eq!(Candidate::from_input("42").num, Some(42.0));
    assert_eq!(Candidate::from_input("TRUE").num, Some(1.0));
    assert_eq!(Candidate::from_input("false").num, Some(0.0));
    assert_eq!(Candidate::from_input("hello").num, None);
    assert!(Candidate::from_input("   ").empty);
}

#[test]
fn candidate_from_value_reads_an_error_cell() {
    let v = crate::Value::Error(crate::ErrorKind::DivZero);
    let cand = Candidate::from_value(&v, "#DIV/0!");
    let rule = RangeValidation::new(r(0, 0, 9, 0), ValueDomain::Any, ValidationRule::None);
    assert_eq!(
        rule.check(&cand, None),
        Some(Violation::ErrorValue(crate::ErrorKind::DivZero))
    );
}
