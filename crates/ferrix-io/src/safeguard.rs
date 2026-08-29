//! Resource safeguards for hostile and malformed files.
//!
//! An `.xlsx` is a zip of XML. Both halves of that sentence are attack
//! surface, and both were previously trusted:
//!
//! - **The zip.** Entry sizes in the central directory are *claims made by
//!   the file*. `Vec::with_capacity(entry.size())` on a 4 GB claim aborts the
//!   process before a single byte is decompressed, and a 40 KB archive that
//!   declares 4 GB of contents is a decompression bomb.
//! - **The XML.** A truncated part used to be indistinguishable from a
//!   complete one, because every reader here matched
//!   `Ok(Event::Eof) | Err(_) => break`. A sheet cut in half imported as a
//!   *shorter sheet*, silently, with no error anywhere.
//!
//! ## The shape of the defence
//!
//! Everything here refuses **before** it allocates, and every refusal names
//! the part that caused it. The three primitives are:
//!
//! 1. [`inspect_archive`] — reads the zip *central directory* only. No entry
//!    is opened and nothing is decompressed. It checks the declared expansion
//!    ratio, the declared total, and every entry name.
//! 2. [`read_part_capped`] — reads one part with a hard ceiling on bytes
//!    actually read, so a lying header buys the attacker nothing. The claim
//!    is checked first (cheap), then the stream is capped (correct).
//! 3. [`xml_reader`] / [`guard_event`] — a `quick_xml::Reader` configured for
//!    hostile input, plus the event-level check that refuses entity
//!    declarations and unresolvable general references.
//!
//! ## The scale invariant is not negotiable
//!
//! None of this reads a file to check it. The bomb check reads the central
//! directory — tens of bytes per entry, independent of the declared sizes.
//! A 10 GB file is admitted or refused without a decompressor being built.
//!
//! ## What is *not* defended here
//!
//! `zip::ZipArchive::new` parses the whole central directory into an index
//! map before this module gets control, so an archive with an enormous entry
//! *count* allocates proportionally to that count. [`Limits::max_entries`]
//! rejects it immediately afterwards, which bounds what is kept, not what was
//! briefly built. Bounding that properly needs a streaming central-directory
//! reader the `zip` crate does not expose.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use ferrix_core::budget::fmt_bytes;
use ferrix_core::{Budget, CancelToken};

/// Largest declared-to-compressed expansion an archive may claim.
///
/// Real spreadsheet XML compresses hard — a sheet of repeated numeric cells
/// reaches 40-60x routinely — so the limit has to be well clear of legitimate
/// content. A zip bomb is not at 60x or 100x; it is at 1000x and up.
pub const DEFAULT_MAX_EXPANSION_RATIO: u64 = 200;

/// Below this declared total the expansion ratio is not applied at all.
///
/// A 200-byte archive that expands to 60 KB is 300x and completely harmless.
/// Ratio only means anything once the absolute number is worth defending.
pub const EXPANSION_FLOOR_BYTES: u64 = 4 << 20;

/// Ceiling on a single part regardless of how much memory the machine has.
/// One XML part larger than this is not a spreadsheet anyone authored.
pub const ABSOLUTE_MAX_PART_BYTES: u64 = 2 << 30;

/// Ceiling on the number of entries in one package.
pub const DEFAULT_MAX_ENTRIES: usize = 100_000;

/// How many XML events pass between cancellation polls.
///
/// Small enough that a cancel is observed in well under a second even on the
/// densest part, large enough that the atomic load does not show up in a
/// profile. A token polled once per file would be decorative.
pub const CANCEL_POLL_EVENTS: usize = 4096;

/// The caps a single import runs under.
///
/// Derived from a measured [`Budget`] in production and constructed directly
/// in tests, so a hostile-input test needs a kilobyte-scale fixture rather
/// than a machine-scale one.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Largest single archive part that may be materialized.
    pub max_part_bytes: u64,
    /// Largest total across every part materialized by one import.
    pub max_total_bytes: u64,
    /// Largest declared-uncompressed / compressed ratio.
    pub max_expansion_ratio: u64,
    /// Declared total below which the ratio check does not apply.
    pub expansion_floor_bytes: u64,
    /// Largest number of entries in the package.
    pub max_entries: usize,
}

impl Limits {
    /// Caps derived from a memory budget.
    pub fn from_budget(b: &Budget) -> Self {
        let claim = b.claim_bytes();
        Self {
            max_part_bytes: claim.min(ABSOLUTE_MAX_PART_BYTES),
            max_total_bytes: claim,
            max_expansion_ratio: DEFAULT_MAX_EXPANSION_RATIO,
            expansion_floor_bytes: EXPANSION_FLOOR_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// Caps from a fresh measurement of this machine. What imports use.
    pub fn measured() -> Self {
        Self::from_budget(&Budget::sample())
    }

    /// Caps with explicit byte ceilings, for tests and for callers that want
    /// to import under a tighter bound than the machine would allow.
    pub fn with_bytes(max_part_bytes: u64, max_total_bytes: u64) -> Self {
        Self {
            max_part_bytes,
            max_total_bytes,
            ..Self::from_budget(&Budget::from_available(u64::MAX))
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::measured()
    }
}

/// Why an import was refused.
///
/// Every variant names the archive and, where one is known, the *part* — the
/// difference between "the file is broken" and "`xl/worksheets/sheet1.xml` is
/// truncated at byte 4096", which is the difference between a bug report and
/// a fix.
#[derive(Debug, thiserror::Error)]
pub enum SafeguardError {
    #[error(
        "{path}: refused before extraction — the archive declares {} of contents \
         from {} of compressed data ({ratio}x expansion, limit {limit}x). \
         This is a decompression bomb, not a spreadsheet.",
        fmt_bytes(*declared), fmt_bytes(*compressed)
    )]
    ZipBomb {
        path: String,
        declared: u64,
        compressed: u64,
        ratio: u64,
        limit: u64,
    },

    #[error(
        "{path}: refused before extraction — the archive declares {} of contents, \
         over the {} limit for this machine.",
        fmt_bytes(*declared), fmt_bytes(*limit)
    )]
    ArchiveTooLarge {
        path: String,
        declared: u64,
        limit: u64,
    },

    #[error("{path}: refused before extraction — the archive holds {count} entries, over the {limit} limit.")]
    TooManyEntries {
        path: String,
        count: usize,
        limit: usize,
    },

    #[error(
        "{path}: refused before extraction — entry {entry:?} escapes the target \
         directory. An archive may not name a path outside where it is unpacked."
    )]
    UnsafeEntryPath { path: String, entry: String },

    #[error(
        "{path}: part {part:?} declares {} , over the {} limit for one part.",
        fmt_bytes(*declared), fmt_bytes(*limit)
    )]
    PartTooLarge {
        path: String,
        part: String,
        declared: u64,
        limit: u64,
    },

    #[error(
        "{path}: part {part:?} is larger than its own header claims — it exceeded \
         the {} read ceiling. The archive's size fields are lying.",
        fmt_bytes(*limit)
    )]
    PartOverran {
        path: String,
        part: String,
        limit: u64,
    },

    #[error(
        "{path}: reading part {part:?} would take the import past its {} memory \
         limit ({} already read). Import abandoned; nothing was kept.",
        fmt_bytes(*limit), fmt_bytes(*so_far)
    )]
    TotalTooLarge {
        path: String,
        part: String,
        so_far: u64,
        limit: u64,
    },

    #[error(
        "{path}: part {part:?} declares XML entities. Entity expansion is refused \
         — a document type definition is how a 1 KB file becomes 3 GB of text."
    )]
    EntityDeclaration { path: String, part: String },

    #[error("{path}: part {part:?} references undefined XML entity {name:?}.")]
    UndefinedEntity {
        path: String,
        part: String,
        name: String,
    },

    #[error("{path}: part {part:?} is malformed at byte {offset}: {detail}")]
    MalformedXml {
        path: String,
        part: String,
        offset: u64,
        detail: String,
    },

    #[error("{path}: reading part {part:?} failed: {detail}")]
    PartUnreadable {
        path: String,
        part: String,
        detail: String,
    },

    #[error("{path}: import cancelled while reading {part:?}; nothing was kept.")]
    Cancelled { path: String, part: String },
}

impl SafeguardError {
    /// The archive part this refusal is about, when one is known.
    ///
    /// Exposed so callers (and tests) can assert on the *named part* rather
    /// than merely on "something failed".
    pub fn part(&self) -> Option<&str> {
        match self {
            SafeguardError::PartTooLarge { part, .. }
            | SafeguardError::PartOverran { part, .. }
            | SafeguardError::TotalTooLarge { part, .. }
            | SafeguardError::EntityDeclaration { part, .. }
            | SafeguardError::UndefinedEntity { part, .. }
            | SafeguardError::MalformedXml { part, .. }
            | SafeguardError::PartUnreadable { part, .. }
            | SafeguardError::Cancelled { part, .. } => Some(part),
            _ => None,
        }
    }

    /// True when the file was refused without anything being extracted.
    pub fn is_pre_extraction(&self) -> bool {
        matches!(
            self,
            SafeguardError::ZipBomb { .. }
                | SafeguardError::ArchiveTooLarge { .. }
                | SafeguardError::TooManyEntries { .. }
                | SafeguardError::UnsafeEntryPath { .. }
        )
    }
}

// ------------------------------------------------------------ zip paths ---

/// Resolve an archive entry name to a path that cannot escape a target
/// directory, or `None` if it tries to.
///
/// Refused: absolute paths (`/etc/passwd`), drive-qualified paths (`C:\x`),
/// UNC-ish prefixes, and any `..` component — including one reached through a
/// backslash separator, which Windows honours as a separator even though the
/// zip specification says names use forward slashes. An attacker writes
/// `..\..\evil` precisely because a naive reader sees one component.
///
/// Note that this normalizes `.` away but never *resolves* `..` against
/// earlier components: `a/../b` is refused rather than rewritten to `b`. A
/// legitimate OOXML package never contains one, and "clever" rewriting is how
/// path-traversal fixes get bypassed.
pub fn safe_entry_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    // Windows honours both separators; normalize so one pass sees every
    // component regardless of which the archive used.
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }
    // `C:` or `C:/...` — a drive-relative or drive-absolute path.
    if normalized.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let mut out = PathBuf::new();
    for seg in normalized.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return None,
            s => out.push(s),
        }
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    // Belt and braces: whatever the platform makes of the pieces, the result
    // must still be relative and free of parent components.
    if out.is_absolute()
        || out.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(out)
}

/// Join an archive entry onto a target directory, refusing traversal.
pub fn safe_extract_path(target_dir: &Path, entry: &str) -> Option<PathBuf> {
    Some(target_dir.join(safe_entry_path(entry)?))
}

// ------------------------------------------------------ archive inspect ---

/// What the central directory claims about an archive.
#[derive(Clone, Debug, Default)]
pub struct ArchiveReport {
    pub entries: usize,
    /// Sum of the declared uncompressed sizes.
    pub declared_bytes: u64,
    /// Sum of the compressed sizes.
    pub compressed_bytes: u64,
    /// Largest single declared uncompressed size.
    pub largest_part_bytes: u64,
    /// Name of the largest part.
    pub largest_part: String,
}

impl ArchiveReport {
    /// Declared expansion ratio, or 1 for an archive that stores its content.
    pub fn expansion_ratio(&self) -> u64 {
        if self.compressed_bytes == 0 {
            return 1;
        }
        self.declared_bytes / self.compressed_bytes
    }
}

/// Vet an archive from its central directory, before anything is extracted.
///
/// This is the zip-bomb gate. It opens no entry, builds no decompressor, and
/// costs the same for a 40 KB bomb as for a 40 KB spreadsheet — which is what
/// makes it usable on the 10 GB files Ferrix targets.
pub fn inspect_archive<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    path: &str,
    limits: &Limits,
) -> Result<ArchiveReport, SafeguardError> {
    let count = zip.len();
    if count > limits.max_entries {
        return Err(SafeguardError::TooManyEntries {
            path: path.to_string(),
            count,
            limit: limits.max_entries,
        });
    }

    let mut report = ArchiveReport {
        entries: count,
        ..Default::default()
    };

    for i in 0..count {
        // `by_index_raw` hands back the central-directory record and a raw
        // (undecompressed) reader. Nothing is inflated here.
        let f = zip
            .by_index_raw(i)
            .map_err(|e| SafeguardError::PartUnreadable {
                path: path.to_string(),
                part: format!("entry {i}"),
                detail: e.to_string(),
            })?;
        let name = f.name().to_string();
        if safe_entry_path(&name).is_none() {
            return Err(SafeguardError::UnsafeEntryPath {
                path: path.to_string(),
                entry: name,
            });
        }
        let declared = f.size();
        report.declared_bytes = report.declared_bytes.saturating_add(declared);
        report.compressed_bytes = report.compressed_bytes.saturating_add(f.compressed_size());
        if declared > report.largest_part_bytes {
            report.largest_part_bytes = declared;
            report.largest_part = name;
        }
    }

    if report.declared_bytes > limits.max_total_bytes {
        return Err(SafeguardError::ArchiveTooLarge {
            path: path.to_string(),
            declared: report.declared_bytes,
            limit: limits.max_total_bytes,
        });
    }

    let ratio = report.expansion_ratio();
    if report.declared_bytes >= limits.expansion_floor_bytes && ratio > limits.max_expansion_ratio {
        return Err(SafeguardError::ZipBomb {
            path: path.to_string(),
            declared: report.declared_bytes,
            compressed: report.compressed_bytes,
            ratio,
            limit: limits.max_expansion_ratio,
        });
    }

    Ok(report)
}

/// Open and vet an `.xlsx`-style package in one step.
pub fn open_checked(
    path: &Path,
    limits: &Limits,
) -> Result<(zip::ZipArchive<std::fs::File>, ArchiveReport), SafeguardError> {
    let disp = path.display().to_string();
    let file = std::fs::File::open(path).map_err(|e| SafeguardError::PartUnreadable {
        path: disp.clone(),
        part: "(package)".to_string(),
        detail: e.to_string(),
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| SafeguardError::PartUnreadable {
        path: disp.clone(),
        part: "(package)".to_string(),
        detail: e.to_string(),
    })?;
    let report = inspect_archive(&mut zip, &disp, limits)?;
    Ok((zip, report))
}

// --------------------------------------------------------- part reading ---

/// Read at most `limit` bytes, refusing rather than growing past it.
///
/// The declared size in a zip header is a claim; this is the enforcement. The
/// reader is capped at `limit + 1` so overrun is *detected* rather than
/// truncated into silently-wrong content — the exact failure mode this whole
/// module exists to remove.
pub fn read_part_capped<R: Read>(
    mut src: R,
    limit: u64,
    path: &str,
    part: &str,
) -> Result<Vec<u8>, SafeguardError> {
    // The +1 is what makes overrun observable: hitting it means the source
    // had more to give, so the claim was false.
    let ceiling = limit.saturating_add(1);
    let mut buf = Vec::new();
    let read = (&mut src)
        .take(ceiling)
        .read_to_end(&mut buf)
        .map_err(|e| SafeguardError::PartUnreadable {
            path: path.to_string(),
            part: part.to_string(),
            detail: e.to_string(),
        })?;
    if read as u64 > limit {
        return Err(SafeguardError::PartOverran {
            path: path.to_string(),
            part: part.to_string(),
            limit,
        });
    }
    Ok(buf)
}

/// A running total across the parts one import materializes.
///
/// Import memory is bounded by this, not by what the file claims. Crossing
/// the line returns an error naming the part that crossed it, and the caller
/// drops everything read so far — partial state is never handed back.
#[derive(Debug)]
pub struct PartBudget<'a> {
    limits: &'a Limits,
    path: String,
    used: u64,
}

impl<'a> PartBudget<'a> {
    pub fn new(path: impl Into<String>, limits: &'a Limits) -> Self {
        Self {
            limits,
            path: path.into(),
            used: 0,
        }
    }

    pub fn used(&self) -> u64 {
        self.used
    }

    /// Read one named part under both the per-part and the running total cap.
    pub fn read<R: Read>(
        &mut self,
        src: R,
        declared: u64,
        part: &str,
    ) -> Result<Vec<u8>, SafeguardError> {
        if declared > self.limits.max_part_bytes {
            return Err(SafeguardError::PartTooLarge {
                path: self.path.clone(),
                part: part.to_string(),
                declared,
                limit: self.limits.max_part_bytes,
            });
        }
        let remaining = self.limits.max_total_bytes.saturating_sub(self.used);
        if declared > remaining {
            return Err(SafeguardError::TotalTooLarge {
                path: self.path.clone(),
                part: part.to_string(),
                so_far: self.used,
                limit: self.limits.max_total_bytes,
            });
        }
        // Cap the actual read at the smaller of the two, so a header that
        // under-declares cannot smuggle bytes past the running total either.
        let cap = declared.min(remaining).min(self.limits.max_part_bytes);
        let buf = read_part_capped(src, cap, &self.path, part)?;
        self.used = self.used.saturating_add(buf.len() as u64);
        Ok(buf)
    }
}

/// Read every file entry of a package into memory, under `limits`.
///
/// Replaces the previous `Vec::with_capacity(entry.size())` loop, which sized
/// an allocation directly from an attacker-controlled field. Entry names are
/// re-checked here as well as in [`inspect_archive`], because this is the
/// function that would hand a traversing name to a later `join`.
pub fn read_all_parts<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    path: &str,
    limits: &Limits,
    cancel: Option<&CancelToken>,
) -> Result<HashMap<String, Vec<u8>>, SafeguardError> {
    let mut budget = PartBudget::new(path, limits);
    let mut parts = HashMap::new();
    for i in 0..zip.len() {
        if let Some(c) = cancel {
            if c.is_cancelled() {
                // `parts` is dropped on the way out: nothing partial escapes.
                return Err(SafeguardError::Cancelled {
                    path: path.to_string(),
                    part: format!("entry {i}"),
                });
            }
        }
        let f = zip
            .by_index(i)
            .map_err(|e| SafeguardError::PartUnreadable {
                path: path.to_string(),
                part: format!("entry {i}"),
                detail: e.to_string(),
            })?;
        if !f.is_file() {
            continue;
        }
        let name = f.name().to_string();
        if safe_entry_path(&name).is_none() {
            return Err(SafeguardError::UnsafeEntryPath {
                path: path.to_string(),
                entry: name,
            });
        }
        let declared = f.size();
        let buf = budget.read(f, declared, &name)?;
        parts.insert(name, buf);
    }
    Ok(parts)
}

// ------------------------------------------------------------------ xml ---

/// A `quick_xml::Reader` configured for input nobody vouches for.
///
/// quick-xml never expands entities on its own — it reports a general
/// reference as [`quick_xml::events::Event::GeneralRef`] and leaves the
/// decision to the caller — so billion-laughs cannot be built out of it by
/// accident. That is a property of the dependency, not of this code, which is
/// exactly why `entity_expansion_is_not_possible` pins it: a future version
/// that "helpfully" resolved DTD entities would otherwise land silently.
///
/// The settings that *are* ours: end tags must match, unmatched ends are
/// ill-formed, and a dangling `&` is an error rather than literal text.
pub fn xml_reader<R: std::io::BufRead>(src: R) -> quick_xml::Reader<R> {
    let mut rd = quick_xml::Reader::from_reader(src);
    let cfg = rd.config_mut();
    cfg.check_end_names = true;
    cfg.allow_unmatched_ends = false;
    cfg.allow_dangling_amp = false;
    rd
}

/// The five entities XML defines without a DTD. Anything else in a
/// spreadsheet part means the file brought its own document type definition.
fn is_predefined_entity(name: &str) -> bool {
    matches!(name, "amp" | "lt" | "gt" | "quot" | "apos")
}

/// Scan a raw attribute value for entity references that would need a DTD.
///
/// Attribute values do **not** produce `GeneralRef` events — they live inside
/// the `BytesStart` and are only decoded if someone asks. So a payload written
/// as `<sheet name="&lol4;"/>` bypassed the event-level check entirely, which
/// is precisely the shape the classic billion-laughs document uses. This walks
/// the raw bytes instead.
///
/// Numeric character references (`&#65;`, `&#x41;`) are single code points and
/// are allowed; a named reference that is not one of the five predefined ones
/// is refused.
fn check_attr_entities(
    e: &quick_xml::events::BytesStart<'_>,
    path: &str,
    part: &str,
) -> Result<(), SafeguardError> {
    for attr in e.attributes().flatten() {
        let v = attr.value.as_ref();
        let mut i = 0usize;
        while let Some(amp) = v[i..].iter().position(|&b| b == b'&') {
            let start = i + amp + 1;
            let Some(semi) = v[start..].iter().position(|&b| b == b';') else {
                break;
            };
            let name = String::from_utf8_lossy(&v[start..start + semi]).into_owned();
            // `&#...;` is a character reference, not an entity.
            if !name.starts_with('#') && !is_predefined_entity(&name) {
                return Err(SafeguardError::UndefinedEntity {
                    path: path.to_string(),
                    part: part.to_string(),
                    name,
                });
            }
            i = start + semi + 1;
            if i >= v.len() {
                break;
            }
        }
    }
    Ok(())
}

/// Vet one parsed event.
///
/// Refuses the three things a hostile XML part uses to turn a small file into
/// a large one: a `<!DOCTYPE ... [<!ENTITY ...>]>` internal subset, a
/// reference to an entity that subset would have defined, and the same
/// reference hidden inside an attribute value where no event reports it.
pub fn guard_event(
    ev: &quick_xml::events::Event<'_>,
    path: &str,
    part: &str,
) -> Result<(), SafeguardError> {
    use quick_xml::events::Event as E;
    match ev {
        E::DocType(_) => Err(SafeguardError::EntityDeclaration {
            path: path.to_string(),
            part: part.to_string(),
        }),
        E::Start(e) | E::Empty(e) => check_attr_entities(e, path, part),
        E::GeneralRef(r) => {
            // A numeric character reference is a single code point and can
            // never expand; only *named* references need a definition.
            if matches!(r.resolve_char_ref(), Ok(Some(_))) {
                return Ok(());
            }
            let name = r.decode().map(|c| c.into_owned()).unwrap_or_default();
            if is_predefined_entity(&name) {
                Ok(())
            } else {
                Err(SafeguardError::UndefinedEntity {
                    path: path.to_string(),
                    part: part.to_string(),
                    name,
                })
            }
        }
        _ => Ok(()),
    }
}

/// Turn a quick-xml error into a refusal that names the part and the offset.
///
/// This is the function whose absence made a truncated sheet import as a
/// short one: the old readers matched `Err(_) => break` and treated a parse
/// failure as end-of-file.
pub fn malformed(e: quick_xml::Error, offset: u64, path: &str, part: &str) -> SafeguardError {
    SafeguardError::MalformedXml {
        path: path.to_string(),
        part: part.to_string(),
        offset,
        detail: e.to_string(),
    }
}

/// Drive an XML part to completion, refusing malformed input and hostile
/// entities, polling cancellation as it goes.
///
/// `on_event` sees every event except `Eof`. Returning `Err` from it stops
/// the scan.
pub fn scan_part<F>(
    xml: &[u8],
    path: &str,
    part: &str,
    cancel: Option<&CancelToken>,
    mut on_event: F,
) -> Result<(), SafeguardError>
where
    F: FnMut(&quick_xml::events::Event<'_>) -> Result<(), SafeguardError>,
{
    let mut rd = xml_reader(xml);
    let mut buf = Vec::new();
    let mut seen = 0usize;
    loop {
        if let Some(c) = cancel {
            seen += 1;
            if seen % CANCEL_POLL_EVENTS == 0 && c.is_cancelled() {
                return Err(SafeguardError::Cancelled {
                    path: path.to_string(),
                    part: part.to_string(),
                });
            }
        }
        let offset = rd.buffer_position();
        match rd.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(ev) => {
                guard_event(&ev, path, part)?;
                on_event(&ev)?;
            }
            Err(e) => return Err(malformed(e, offset, path, part)),
        }
        buf.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests;
