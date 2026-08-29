//! Round-trip tests for the `.fxnotes` sidecar.

use super::*;

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ferrix_notes_{}_{}_{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

fn cell(r: u32, c: u32) -> CellRef {
    CellRef::new(r, c)
}

#[test]
fn sidecar_path_appends_rather_than_substituting() {
    // with_extension() would turn sales.ferrix into sales.fxnotes and collide
    // with a different base's sidecar.
    let p = comments_path_for(Path::new("data/sales.ferrix"));
    assert!(p.to_string_lossy().ends_with("sales.ferrix.fxnotes"));
}

#[test]
fn missing_sidecar_is_not_an_error() {
    let p = tmp("absent.fxnotes");
    assert!(load_comments(&p).expect("load").is_none());
}

#[test]
fn comments_round_trip_through_save_and_reload() {
    let mut want = CommentMap::new();
    want.set(cell(0, 0), Comment::new("ana", "first cell"));
    want.set(cell(7, 3), Comment::new("bo", "multi\nline note"));
    want.set(cell(7, 9), Comment::new("", "no author at all"));
    want.set(
        cell(42, 1),
        Comment::new("cy", "unicode: naïve café 日本語"),
    );

    let p = tmp("roundtrip.fxnotes");
    let size = save_comments(&p, &want).expect("save");
    assert!(size > 0, "a non-empty map must write bytes");

    let got = load_comments(&p).expect("load").expect("file exists");
    assert_eq!(got.len(), 4);
    assert_eq!(got, want, "every comment must survive the round trip");
    // Spot-check the payloads rather than trusting only structural equality.
    assert_eq!(got.get(cell(7, 3)).unwrap().text, "multi\nline note");
    assert_eq!(got.get(cell(42, 1)).unwrap().author, "cy");
    assert_eq!(got.get(cell(7, 9)).unwrap().author, "");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn saving_an_empty_map_removes_a_stale_sidecar() {
    // Deleting the last comment must not leave a file behind that outlives the
    // data it described.
    let p = tmp("emptied.fxnotes");
    let mut m = CommentMap::new();
    m.set(cell(1, 1), Comment::new("ana", "temporary"));
    save_comments(&p, &m).expect("save");
    assert!(p.exists());

    m.remove(cell(1, 1));
    save_comments(&p, &m).expect("save empty");
    assert!(
        !p.exists(),
        "an emptied map must not leave a sidecar behind"
    );
    assert!(load_comments(&p).expect("load").is_none());
}

#[test]
fn saves_are_byte_reproducible() {
    let mut m = CommentMap::new();
    for (r, c) in [(9u32, 2u32), (0, 5), (9, 0), (3, 3)] {
        m.set(cell(r, c), Comment::new("ana", format!("note {r}/{c}")));
    }
    let a = tmp("repro_a.fxnotes");
    let b = tmp("repro_b.fxnotes");
    save_comments(&a, &m).expect("save a");
    save_comments(&b, &m).expect("save b");
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "two saves of one map must be byte-identical"
    );
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn garbage_is_rejected_rather_than_misparsed() {
    let p = tmp("garbage.fxnotes");
    std::fs::write(&p, b"this is not a ferrix comments file at all").unwrap();
    assert!(matches!(
        load_comments(&p),
        Err(CommentSidecarError::BadMagic)
    ));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_truncated_file_is_detected_not_silently_shortened() {
    // The failure this guards against: a half-written file loading as "you
    // had two comments" when the user saved five.
    let mut m = CommentMap::new();
    for i in 0..5u32 {
        m.set(
            cell(i, 0),
            Comment::new("ana", "a reasonably long note body"),
        );
    }
    let p = tmp("trunc.fxnotes");
    save_comments(&p, &m).expect("save");
    let bytes = std::fs::read(&p).unwrap();
    std::fs::write(&p, &bytes[..bytes.len() / 2]).unwrap();

    assert!(
        matches!(load_comments(&p), Err(CommentSidecarError::Truncated)),
        "a truncated sidecar must error rather than return fewer comments"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_future_version_is_refused() {
    let mut m = CommentMap::new();
    m.set(cell(0, 0), Comment::new("a", "b"));
    let p = tmp("version.fxnotes");
    save_comments(&p, &m).expect("save");
    let mut bytes = std::fs::read(&p).unwrap();
    bytes[8] = 99; // version field, little-endian low byte
    std::fs::write(&p, &bytes).unwrap();
    assert!(matches!(
        load_comments(&p),
        Err(CommentSidecarError::BadVersion(99))
    ));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn three_comments_on_a_200m_row_sheet_write_a_tiny_file() {
    // O(comments), not O(rows): the whole reason the store is sparse.
    let mut m = CommentMap::new();
    m.set(cell(0, 0), Comment::new("ana", "top"));
    m.set(cell(99_999_999, 4), Comment::new("bo", "middle"));
    m.set(cell(199_999_999, 9), Comment::new("cy", "bottom"));
    let p = tmp("huge.fxnotes");
    let size = save_comments(&p, &m).expect("save");
    assert!(
        size < 512,
        "three comments over 200M rows wrote {size} bytes"
    );
    let got = load_comments(&p).expect("load").unwrap();
    assert_eq!(got.len(), 3);
    // Deep row indices must survive the u32 round trip exactly.
    assert_eq!(got.get(cell(199_999_999, 9)).unwrap().text, "bottom");
    let _ = std::fs::remove_file(&p);
}
