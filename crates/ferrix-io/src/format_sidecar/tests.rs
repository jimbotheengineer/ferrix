//! Round-trip tests for the `.fxfmt` sidecar.

use super::*;
use ferrix_core::format::presets;

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ferrix_fmt_{}_{}_{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

/// A store exercising every scope and every rule kind, so a round-trip test
/// over it covers the whole encoding rather than the happy path.
fn kitchen_sink() -> SheetFormat {
    let mut f = SheetFormat::new();

    f.set_column_format(
        0,
        NumberFormat::Currency {
            symbol: "£".into(),
            places: 2,
        },
    );
    f.set_column_manual(
        0,
        ManualStyle {
            fill: Some(Rgb(0xEE, 0xEE, 0xF5)),
            text: None,
            typography: Default::default(),
        },
    );
    f.push_column_rule(0, presets::sign_colors());
    f.push_column_rule(0, presets::above(1000.0));
    f.push_column_rule(0, presets::below(-1000.0));
    f.push_column_rule(0, presets::color_scale());
    f.push_column_rule(
        0,
        ConditionalRule::ColorScale3 {
            min: Rgb(1, 2, 3),
            mid: Rgb(4, 5, 6),
            max: Rgb(7, 8, 9),
        },
    );
    f.push_column_rule(0, presets::data_bar());
    f.push_column_rule(0, presets::top_n(25));
    f.push_column_rule(0, presets::bottom_n(3));
    f.push_column_rule(0, presets::contains("ERR"));

    f.set_column_format(3, NumberFormat::Date(DateStyle::Euro));
    f.set_column_format(4, NumberFormat::Custom("[$€-407]#,##0.00".into()));
    f.set_column_format(5, NumberFormat::Percent { places: 3 });

    // A range covering 200M rows, to prove size is about rules not rows.
    let i = f.push_range(
        RangeFormat::new(TableRange::new(0, 2, 199_999_999, 2))
            .with_rule(ConditionalRule::Manual {
                fill: Some(Rgb(10, 20, 30)),
                text: Some(Rgb(40, 50, 60)),
                typography: Default::default(),
            })
            .with_rule(ConditionalRule::Threshold {
                op: CmpOp::Ne,
                value: 0.0,
                fill: Rgb(1, 1, 1),
                text: Rgb(2, 2, 2),
            }),
    );
    f.range_mut(i).unwrap().format = Some(NumberFormat::Thousands { places: 1 });

    f.set_cell_override(
        CellRef::new(42, 7),
        CellOverride {
            manual: ManualStyle {
                fill: Some(Rgb(255, 255, 0)),
                text: Some(Rgb(0, 0, 0)),
                // Every typography field set to a NON-default value, so the
                // round-trip proves each one is actually written and read
                // back rather than silently defaulting on both sides.
                typography: ferrix_core::format::Typography {
                    family: Some(ferrix_core::format::FontFamily::Monospace),
                    size: Some(15.25),
                    bold: Some(true),
                    italic: Some(true),
                    underline: Some(true),
                    strikethrough: Some(false),
                },
            },
            format: Some(NumberFormat::Decimal { places: 4 }),
        },
    );
    f
}

#[test]
fn every_scope_and_rule_kind_survives_a_round_trip() {
    let want = kitchen_sink();
    let p = tmp("roundtrip.fxfmt");
    save_format(&p, &want).expect("save");
    let got = load_format(&p).expect("load").expect("file exists");
    std::fs::remove_file(&p).ok();

    assert_eq!(
        got, want,
        "the reloaded formatting must equal what was saved, field for field"
    );
}

#[test]
fn rule_order_survives_a_round_trip() {
    // Order is semantics here — a later rule overrides an earlier one — so a
    // round trip that preserved the rules but shuffled them would silently
    // change what the user sees.
    let mut want = SheetFormat::new();
    for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
        want.push_column_rule(0, presets::above(v));
    }
    let p = tmp("order.fxfmt");
    save_format(&p, &want).expect("save");
    let got = load_format(&p).expect("load").unwrap();
    std::fs::remove_file(&p).ok();

    let labels: Vec<String> = got
        .column(0)
        .unwrap()
        .rules
        .iter()
        .map(|r| r.label())
        .collect();
    assert_eq!(
        labels,
        vec![
            "Value > 1".to_string(),
            "Value > 2".into(),
            "Value > 3".into(),
            "Value > 4".into(),
            "Value > 5".into()
        ]
    );
}

#[test]
fn a_custom_number_format_is_preserved_byte_for_byte() {
    // The contract NumberFormat::from_code makes: a format we cannot render is
    // still a format we must never lose.
    let raw = "[$€-407]#,##0.00;[RED]-#,##0.00";
    let mut want = SheetFormat::new();
    want.set_column_format(1, NumberFormat::Custom(raw.into()));
    let p = tmp("custom.fxfmt");
    save_format(&p, &want).expect("save");
    let got = load_format(&p).expect("load").unwrap();
    std::fs::remove_file(&p).ok();

    assert_eq!(
        got.number_format(CellRef::new(0, 1)),
        Some(&NumberFormat::Custom(raw.into()))
    );
}

#[test]
fn a_200m_row_range_writes_a_tiny_file() {
    let mut f = SheetFormat::new();
    f.set_range_manual(
        TableRange::new(0, 0, 199_999_999, 20),
        ManualStyle {
            fill: Some(Rgb(1, 2, 3)),
            text: None,
            typography: Default::default(),
        },
    );
    let p = tmp("huge.fxfmt");
    let size = save_format(&p, &f).expect("save");
    std::fs::remove_file(&p).ok();
    assert!(
        size < 128,
        "formatting 4 billion cells wrote {size} bytes; the file must be \
         O(rules), not O(cells)"
    );
}

#[test]
fn saving_twice_produces_identical_bytes() {
    // Reproducibility matters for diffing and backup dedup; it is why
    // SheetFormat keys on BTreeMap rather than HashMap.
    let f = kitchen_sink();
    let a = tmp("repro_a.fxfmt");
    let b = tmp("repro_b.fxfmt");
    save_format(&a, &f).expect("save a");
    save_format(&b, &f).expect("save b");
    let (ba, bb) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
    assert_eq!(ba, bb, "two saves of the same state must be byte-identical");
}

#[test]
fn an_empty_store_round_trips_as_empty() {
    let p = tmp("empty.fxfmt");
    save_format(&p, &SheetFormat::new()).expect("save");
    let got = load_format(&p).expect("load").unwrap();
    std::fs::remove_file(&p).ok();
    assert!(got.is_empty());
}

#[test]
fn a_missing_sidecar_is_not_an_error() {
    let p = tmp("absent.fxfmt");
    assert!(
        load_format(&p).expect("no error").is_none(),
        "a dataset nobody has formatted is the common case, not a failure"
    );
}

#[test]
fn a_corrupt_file_is_rejected_rather_than_misparsed() {
    let p = tmp("bad.fxfmt");
    std::fs::write(&p, b"NOTFERRIXnonsense").unwrap();
    let e = load_format(&p).expect_err("must reject");
    std::fs::remove_file(&p).ok();
    assert!(matches!(e, FormatSidecarError::BadMagic), "got {e:?}");
}

#[test]
fn a_truncated_file_is_detected() {
    let f = kitchen_sink();
    let p = tmp("trunc.fxfmt");
    save_format(&p, &f).expect("save");
    let bytes = std::fs::read(&p).unwrap();
    std::fs::write(&p, &bytes[..bytes.len() / 2]).unwrap();
    let e = load_format(&p).expect_err("must reject");
    std::fs::remove_file(&p).ok();
    assert!(
        matches!(
            e,
            FormatSidecarError::Truncated | FormatSidecarError::UnknownTag(_)
        ),
        "a half-written file must fail loudly, got {e:?}"
    );
}

#[test]
fn a_future_version_is_refused_rather_than_guessed_at() {
    let p = tmp("future.fxfmt");
    save_format(&p, &SheetFormat::new()).expect("save");
    let mut bytes = std::fs::read(&p).unwrap();
    bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
    std::fs::write(&p, &bytes).unwrap();
    let e = load_format(&p).expect_err("must reject");
    std::fs::remove_file(&p).ok();
    assert!(matches!(e, FormatSidecarError::BadVersion(99)), "got {e:?}");
}

#[test]
fn the_sidecar_path_hangs_off_the_base_name() {
    let p = format_path_for(Path::new("/data/sales.ferrix"));
    assert_eq!(p.file_name().unwrap(), "sales.ferrix.fxfmt");
}

#[test]
fn formatting_does_not_carry_a_base_fingerprint() {
    // Unlike .fxedits, formatting must survive the base being regenerated:
    // "column 2 is currency" stays true when the data refreshes with more
    // rows. This test pins that decision — it fails if someone adds a
    // staleness check that would throw the user's rules away on a refresh.
    let mut f = SheetFormat::new();
    f.set_column_format(2, NumberFormat::Percent { places: 1 });
    let p = tmp("nofingerprint.fxfmt");
    let size = save_format(&p, &f).expect("save");
    // Header is magic (8) + version (4) + three counts (12) = 24 bytes. A
    // fingerprint would add at least 28 more before any rule data.
    assert!(
        size < 64,
        "the file is {size} bytes; a base fingerprint appears to have crept in"
    );
    let got = load_format(&p).expect("load with no base present at all");
    std::fs::remove_file(&p).ok();
    assert!(got.is_some());
}
