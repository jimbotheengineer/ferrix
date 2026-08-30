//! Tests for the `.fxpivot` sidecar.

use super::*;
use std::path::PathBuf;

/// A scratch path under the OS temp dir, unique per test, cleaned up on drop.
struct TempPath(PathBuf);

impl TempPath {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let unique = format!(
            "ferrix_pivot_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(unique);
        TempPath(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut tmp = self.0.as_os_str().to_os_string();
        tmp.push(".tmp");
        let _ = std::fs::remove_file(PathBuf::from(tmp));
    }
}

fn sample() -> Vec<PivotRecord> {
    vec![
        PivotRecord {
            sheet_id: 3,
            source_id: 0,
            auto_refresh: false,
            group_by: vec![0, 2],
            values: vec![(1, agg_code("Sum").unwrap()), (1, agg_code("Avg").unwrap())],
        },
        PivotRecord {
            sheet_id: 5,
            source_id: 1,
            auto_refresh: true,
            group_by: vec![],
            values: vec![(4, agg_code("Count").unwrap())],
        },
    ]
}

#[test]
fn round_trips_bindings_intact() {
    let t = TempPath::new("roundtrip");
    let records = sample();
    let bytes = save_pivots(t.path(), &records).unwrap();
    assert!(bytes > 0, "a non-empty set writes a real file");

    let loaded = load_pivots(t.path()).unwrap().expect("sidecar present");
    assert_eq!(
        loaded, records,
        "spec must survive the round trip byte-for-byte"
    );
}

#[test]
fn missing_file_is_none_not_error() {
    let t = TempPath::new("missing");
    // Nothing written.
    assert!(load_pivots(t.path()).unwrap().is_none());
}

#[test]
fn empty_set_deletes_the_sidecar() {
    let t = TempPath::new("empty");
    save_pivots(t.path(), &sample()).unwrap();
    assert!(t.path().exists(), "the file exists after a real save");
    // Saving an empty list must retire the stale file, not leave a zero-record
    // artefact that reloads as "no pivots" while sitting on disk.
    let n = save_pivots(t.path(), &[]).unwrap();
    assert_eq!(n, 0);
    assert!(!t.path().exists(), "empty save removes the sidecar");
    assert!(load_pivots(t.path()).unwrap().is_none());
}

#[test]
fn save_is_byte_reproducible() {
    let t1 = TempPath::new("repro1");
    let t2 = TempPath::new("repro2");
    save_pivots(t1.path(), &sample()).unwrap();
    save_pivots(t2.path(), &sample()).unwrap();
    let a = std::fs::read(t1.path()).unwrap();
    let b = std::fs::read(t2.path()).unwrap();
    assert_eq!(a, b, "two saves of the same bindings are byte-identical");
}

#[test]
fn bad_magic_is_rejected() {
    let t = TempPath::new("magic");
    std::fs::write(t.path(), b"NOTAPIVOTxxxxxxxx").unwrap();
    assert!(matches!(
        load_pivots(t.path()),
        Err(PivotSidecarError::BadMagic)
    ));
}

#[test]
fn wrong_version_is_rejected() {
    let t = TempPath::new("version");
    let mut buf = Vec::new();
    buf.extend_from_slice(PIVOT_MAGIC);
    buf.extend_from_slice(&99u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(t.path(), &buf).unwrap();
    assert!(matches!(
        load_pivots(t.path()),
        Err(PivotSidecarError::BadVersion(99))
    ));
}

#[test]
fn truncated_file_is_detected() {
    let t = TempPath::new("trunc");
    save_pivots(t.path(), &sample()).unwrap();
    let full = std::fs::read(t.path()).unwrap();
    // Lop off the tail: the record count still promises more than survives.
    std::fs::write(t.path(), &full[..full.len() - 3]).unwrap();
    assert!(matches!(
        load_pivots(t.path()),
        Err(PivotSidecarError::Truncated)
    ));
}

#[test]
fn unknown_aggregate_code_is_rejected() {
    let t = TempPath::new("badagg");
    let mut buf = Vec::new();
    buf.extend_from_slice(PIVOT_MAGIC);
    buf.extend_from_slice(&PIVOT_VERSION.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes()); // one record
    buf.extend_from_slice(&7u32.to_le_bytes()); // sheet_id
    buf.extend_from_slice(&0u32.to_le_bytes()); // source_id
    buf.push(0); // auto_refresh
    buf.extend_from_slice(&0u32.to_le_bytes()); // no group_by
    buf.extend_from_slice(&1u32.to_le_bytes()); // one value
    buf.extend_from_slice(&0u32.to_le_bytes()); // col 0
    buf.push(200); // bogus aggregate code
    std::fs::write(t.path(), &buf).unwrap();
    assert!(matches!(
        load_pivots(t.path()),
        Err(PivotSidecarError::BadAgg(200))
    ));
}

#[test]
fn every_aggregate_code_round_trips_by_name() {
    for name in ["Sum", "Count", "Avg", "Min", "Max", "StdDev"] {
        let code = agg_code(name).expect("known aggregate");
        assert_eq!(agg_name(code), Some(name), "code<->name is an inverse pair");
    }
    assert!(agg_code("Nonsense").is_none());
    assert!(agg_name(250).is_none());
}

#[test]
fn path_appends_extension_not_substitutes() {
    let p = pivot_path_for(Path::new("sales.ferrix"));
    assert_eq!(p, PathBuf::from("sales.ferrix.fxpivot"));
}
