//! Round-trip and robustness tests for the `.fxpage` sidecar.

use super::*;
use ferrix_core::page::{Orientation, PageOrder, PaperSize, Scaling};

/// A scratch path removed on drop, so a failing assertion still cleans up.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("ferrix-page-{tag}-{uniq}.fxpage"));
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

/// A page state exercising every field: non-default paper/orientation/order,
/// custom margins, FitTo scaling with one axis unset, both repeat ranges, both
/// header and footer with field codes, manual row and column breaks, and a
/// print area.
fn fixture() -> PageState {
    let mut setup = PageSetup {
        paper: PaperSize::A4,
        orientation: Orientation::Landscape,
        margins: Margins::narrow(),
        scaling: Scaling::FitTo {
            wide: Some(1),
            tall: None,
        },
        repeat_rows: Some((0, 2)),
        repeat_cols: Some((0, 0)),
        gridlines: true,
        headings: true,
        header: HeaderFooter {
            left: "Report".into(),
            center: "&F".into(),
            right: "&P/&N".into(),
        },
        footer: HeaderFooter {
            left: String::new(),
            center: "&D &T".into(),
            right: String::new(),
        },
        order: PageOrder::OverThenDown,
        row_breaks: Vec::new(),
        col_breaks: Vec::new(),
    };
    setup.add_row_break(100);
    setup.add_row_break(250);
    setup.add_col_break(5);
    PageState {
        setup,
        print_area: Some(TableRange::new(2, 1, 40, 8)),
    }
}

#[test]
fn round_trips_every_field() {
    let t = Temp::new("roundtrip");
    let original = fixture();
    save_page(t.path(), &original).unwrap();
    let back = load_page(t.path()).unwrap().expect("sidecar should load");
    assert_eq!(
        back, original,
        "the reloaded page state must equal what was saved"
    );
}

#[test]
fn default_state_round_trips() {
    // The common case: nothing was customised. Defaults must survive so a
    // reopen does not silently swap Letter for something else.
    let t = Temp::new("defaults");
    let original = PageState::default();
    save_page(t.path(), &original).unwrap();
    let back = load_page(t.path()).unwrap().unwrap();
    assert_eq!(back, original);
    assert_eq!(back.setup.paper, PaperSize::Letter);
    assert!(back.print_area.is_none());
}

#[test]
fn manual_page_breaks_survive() {
    // Manual breaks set via Page Break Preview live inside PageSetup, so this
    // sidecar carries them for free — assert that explicitly.
    let t = Temp::new("breaks");
    save_page(t.path(), &fixture()).unwrap();
    let back = load_page(t.path()).unwrap().unwrap();
    assert_eq!(back.setup.row_breaks, vec![100, 250]);
    assert_eq!(back.setup.col_breaks, vec![5]);
}

#[test]
fn print_area_survives() {
    let t = Temp::new("printarea");
    save_page(t.path(), &fixture()).unwrap();
    let back = load_page(t.path()).unwrap().unwrap();
    assert_eq!(back.print_area, Some(TableRange::new(2, 1, 40, 8)));
}

#[test]
fn header_footer_field_codes_survive_verbatim() {
    let t = Temp::new("hf");
    save_page(t.path(), &fixture()).unwrap();
    let back = load_page(t.path()).unwrap().unwrap();
    assert_eq!(back.setup.header.right, "&P/&N");
    assert_eq!(back.setup.footer.center, "&D &T");
    assert_eq!(back.setup.footer.left, "");
}

#[test]
fn scaling_fit_to_with_unset_axis_survives() {
    let t = Temp::new("scaling");
    save_page(t.path(), &fixture()).unwrap();
    let back = load_page(t.path()).unwrap().unwrap();
    assert_eq!(
        back.setup.scaling,
        Scaling::FitTo {
            wide: Some(1),
            tall: None
        },
        "FitTo must round-trip with tall left unconstrained, not coerced to Some(0)"
    );
}

#[test]
fn a_huge_print_area_stays_tiny_on_disk() {
    // Persistence is O(fields), never O(cells): a print area over a 200M-row
    // sheet writes a fixed handful of bytes.
    let t = Temp::new("scale");
    let state = PageState {
        setup: PageSetup::default(),
        print_area: Some(TableRange::new(0, 0, 200_000_000, 16_000)),
    };
    let bytes = save_page(t.path(), &state).unwrap();
    assert!(
        bytes < 256,
        "a print area over 200M rows wrote {bytes} bytes; storage must be \
         O(fields), not O(cells)"
    );
    let back = load_page(t.path()).unwrap().unwrap();
    assert_eq!(back.print_area, state.print_area);
}

#[test]
fn missing_file_is_not_an_error() {
    let t = Temp::new("absent");
    assert!(
        load_page(t.path()).unwrap().is_none(),
        "no sidecar means default page setup, not a failure"
    );
}

#[test]
fn bad_magic_is_refused() {
    let t = Temp::new("magic");
    std::fs::write(t.path(), b"NOTFXPAGbytesbytesbytes").unwrap();
    assert!(matches!(
        load_page(t.path()),
        Err(PageSidecarError::BadMagic)
    ));
}

#[test]
fn truncated_file_is_detected_not_misparsed() {
    let t = Temp::new("trunc");
    save_page(t.path(), &fixture()).unwrap();
    let mut bytes = std::fs::read(t.path()).unwrap();
    bytes.truncate(bytes.len() - 7);
    std::fs::write(t.path(), &bytes).unwrap();
    assert!(
        matches!(load_page(t.path()), Err(PageSidecarError::Truncated)),
        "a truncated sidecar must be reported, never silently half-read"
    );
}

#[test]
fn saving_twice_produces_identical_bytes() {
    let a = Temp::new("repro-a");
    let b = Temp::new("repro-b");
    let s = fixture();
    save_page(a.path(), &s).unwrap();
    save_page(b.path(), &s).unwrap();
    assert_eq!(
        std::fs::read(a.path()).unwrap(),
        std::fs::read(b.path()).unwrap(),
        "two saves of the same state must be byte-identical"
    );
}

#[test]
fn an_out_of_range_paper_discriminant_is_refused() {
    let t = Temp::new("badenum");
    save_page(t.path(), &fixture()).unwrap();
    let mut b = std::fs::read(t.path()).unwrap();
    // Paper is the first byte after magic(8) + version(4) = offset 12.
    b[12] = 200;
    std::fs::write(t.path(), &b).unwrap();
    assert!(
        matches!(load_page(t.path()), Err(PageSidecarError::BadEnum)),
        "a paper discriminant past the enum must be refused, not silently mapped"
    );
}

/// A crafted break count must be an error, not an allocation abort.
///
/// Same class as the sizing sidecar's `Vec::with_capacity` bug (#58): a
/// 0xFFFFFFFF count times 4 bytes would reserve 16GB before the read loop can
/// fail. The list grows per record instead, so this is refused as Truncated.
#[test]
fn an_oversized_break_count_is_an_error_not_an_allocation_abort() {
    let t = Temp::new("evil-breaks");
    // A minimal fixture with no header/footer text keeps the row_breaks count
    // at a known-findable position, but locating it byte-exactly is fragile;
    // instead corrupt via a re-save with a controlled tail. Simpler: append a
    // giant count by rewriting the file's row_breaks count in place is hard,
    // so build the malicious bytes directly through the writer then patch the
    // FIRST u32 list count we can reach — the row_breaks length. It sits right
    // after the two header/footer blocks; find it by searching for our unique
    // marker is overkill. Use an empty-text fixture so offsets are stable.
    let mut setup = PageSetup::default();
    setup.add_row_break(7);
    let state = PageState {
        setup,
        print_area: None,
    };
    save_page(t.path(), &state).unwrap();
    let mut b = std::fs::read(t.path()).unwrap();

    // Header: magic(8) + version(4) + paper(1) + orient(1) + order(1) +
    // flags(1) + margins(24) + scaling(Percent => 1+2=3) +
    // repeat_rows(1, None) + repeat_cols(1, None) +
    // header(3 empty strings => 12) + footer(12) = 8+4+4+24+3+1+1+12+12 = 69.
    // row_breaks count u32 sits at offset 69.
    let off = 69usize;
    assert!(b.len() >= off + 4, "row_breaks count must be present");
    // Sanity: the default fixture has exactly one row break.
    assert_eq!(
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap()),
        1,
        "offset check: row_breaks count should be 1 before corruption"
    );
    b[off..off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(t.path(), &b).unwrap();

    let r = load_page(t.path());
    assert!(
        matches!(r, Err(PageSidecarError::Truncated)),
        "a break count far past the file's end must be refused, not reserve 16GB; got {r:?}"
    );
}

/// A header/footer string length that overflows the cursor bounds check must
/// be rejected, not slice-panic — attacker-controlled data on file load.
#[test]
fn an_oversized_string_length_is_rejected() {
    let t = Temp::new("evil-str");
    save_page(t.path(), &fixture()).unwrap();
    let mut b = std::fs::read(t.path()).unwrap();
    // header.left length is the first u32 after the fixed prefix. Prefix:
    // magic(8)+version(4)+paper(1)+orient(1)+order(1)+flags(1)+margins(24) = 40,
    // then scaling. The fixture uses FitTo => 1 + (1+2)+(1+2) = 7 bytes, then
    // repeat_rows Some => 1+8 = 9, repeat_cols Some => 1+8 = 9. So header.left
    // length is at 40 + 7 + 9 + 9 = 65.
    let off = 65usize;
    assert!(b.len() >= off + 4);
    b[off..off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(t.path(), &b).unwrap();
    let r = load_page(t.path());
    assert!(
        matches!(r, Err(PageSidecarError::Truncated)),
        "an oversized string length must be refused, not slice-panic; got {r:?}"
    );
}
