//! Round-trip tests for the `.fxsize` sidecar.

use super::*;
use ferrix_core::sizing::MAX_OUTLINE_LEVEL;

/// A scratch path removed on drop, so a failing assertion still cleans up.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("ferrix-size-{tag}-{uniq}.fxsize"));
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn fixture() -> SheetSizing {
    let mut s = SheetSizing::new();
    s.rows.set_range(0, 9, 30.0);
    s.rows.hide(20, 25);
    s.rows.set(100, 44.5);
    s.cols.set_width(0, 210.0);
    s.cols.set_width(3, 64.0);
    s.cols.hide(2);
    s.row_outline.group(50, 150).unwrap();
    s.row_outline.group(60, 90).unwrap();
    s.row_outline.toggle_at(60);
    s.col_outline.group(4, 8).unwrap();
    s
}

#[test]
fn round_trips_every_field() {
    let t = Temp::new("roundtrip");
    let original = fixture();
    save_sizing(t.path(), &original).unwrap();
    let back = load_sizing(t.path()).unwrap().expect("sidecar should load");
    assert_eq!(
        back, original,
        "the reloaded sizing must equal what was saved"
    );
}

#[test]
fn heights_and_hiding_survive() {
    let t = Temp::new("heights");
    save_sizing(t.path(), &fixture()).unwrap();
    let back = load_sizing(t.path()).unwrap().unwrap();
    assert_eq!(back.rows.height_of(5), Some(30.0));
    assert_eq!(back.rows.height_of(100), Some(44.5));
    assert!(
        back.rows.is_hidden(22),
        "a hidden span must come back hidden"
    );
    assert!(!back.rows.is_hidden(19));
    assert_eq!(back.cols.width_of(0), Some(210.0));
    assert!(back.cols.is_hidden(2));
}

#[test]
fn outline_levels_and_collapse_survive() {
    let t = Temp::new("outline");
    save_sizing(t.path(), &fixture()).unwrap();
    let back = load_sizing(t.path()).unwrap().unwrap();
    // The nesting must come back as it was — NOT re-derived, which would have
    // handed the inner group level 1 if it were replayed first.
    assert_eq!(
        back.row_outline.level_at(70),
        2,
        "inner group keeps level 2"
    );
    assert_eq!(back.row_outline.level_at(120), 1);
    let collapsed: Vec<_> = back.row_outline.collapsed_spans().collect();
    assert_eq!(
        collapsed,
        vec![(61, 90)],
        "the collapsed group must still be collapsed after a reload"
    );
    assert_eq!(back.col_outline.level_at(6), 1);
}

#[test]
fn hidden_rows_index_rebuilds_identically() {
    let t = Temp::new("index");
    let original = fixture();
    save_sizing(t.path(), &original).unwrap();
    let back = load_sizing(t.path()).unwrap().unwrap();
    assert_eq!(
        back.hidden_rows(),
        original.hidden_rows(),
        "the folded hidden-row index must be identical after a reload, or the \
         view would resolve different rows than before the save"
    );
}

#[test]
fn a_huge_hidden_span_stays_tiny_on_disk() {
    let t = Temp::new("scale");
    let mut s = SheetSizing::new();
    s.rows.hide(1, 200_000_000);
    s.row_outline.group(0, 199_999_999).unwrap();
    let bytes = save_sizing(t.path(), &s).unwrap();
    assert!(
        bytes < 256,
        "hiding and grouping 200M rows wrote {bytes} bytes; storage must be \
         O(spans), not O(rows)"
    );
    let back = load_sizing(t.path()).unwrap().unwrap();
    assert!(back.rows.is_hidden(123_456_789));
}

#[test]
fn missing_file_is_not_an_error() {
    let t = Temp::new("absent");
    assert!(
        load_sizing(t.path()).unwrap().is_none(),
        "no sidecar means no sizing, not a failure"
    );
}

#[test]
fn bad_magic_is_refused() {
    let t = Temp::new("magic");
    std::fs::write(t.path(), b"NOTFXSIZbytesbytesbytes").unwrap();
    assert!(matches!(
        load_sizing(t.path()),
        Err(SizeSidecarError::BadMagic)
    ));
}

#[test]
fn truncated_file_is_detected_not_misparsed() {
    let t = Temp::new("trunc");
    save_sizing(t.path(), &fixture()).unwrap();
    let mut bytes = std::fs::read(t.path()).unwrap();
    bytes.truncate(bytes.len() - 7);
    std::fs::write(t.path(), &bytes).unwrap();
    assert!(
        matches!(load_sizing(t.path()), Err(SizeSidecarError::Truncated)),
        "a truncated sidecar must be reported, never silently half-read"
    );
}

#[test]
fn saving_twice_produces_identical_bytes() {
    let a = Temp::new("repro-a");
    let b = Temp::new("repro-b");
    let s = fixture();
    save_sizing(a.path(), &s).unwrap();
    save_sizing(b.path(), &s).unwrap();
    assert_eq!(
        std::fs::read(a.path()).unwrap(),
        std::fs::read(b.path()).unwrap(),
        "two saves of the same state must be byte-identical"
    );
}

#[test]
fn corrupt_level_is_clamped_not_trusted() {
    // A level past the supported depth would leave the gutter with no indent
    // to draw. Loading clamps rather than propagating it.
    let t = Temp::new("clamp");
    let mut s = SheetSizing::new();
    s.row_outline = Outline::from_groups([OutlineGroup {
        first: 0,
        last: 10,
        level: 200,
        collapsed: false,
    }]);
    save_sizing(t.path(), &s).unwrap();
    let back = load_sizing(t.path()).unwrap().unwrap();
    assert!(back.row_outline.max_level() <= MAX_OUTLINE_LEVEL);
}
