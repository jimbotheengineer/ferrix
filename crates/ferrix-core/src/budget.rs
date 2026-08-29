//! Measured memory budget.
//!
//! ## Why this is not a table of constants
//!
//! Every cap in Ferrix used to be a number somebody picked once: one million
//! clipboard cells, two million chart rows, a two-gigabyte string arena. Those
//! numbers are wrong in both directions. On a 4 GB laptop a one-million-cell
//! copy is an out-of-memory kill; on a 128 GB workstation refusing a
//! three-million-row chart is an insult. Neither machine was ever asked.
//!
//! So the caps are derived instead. At startup — and again before anything
//! large — we ask the operating system how much memory is *actually available
//! right now*, subtract a reserve so the rest of the desktop keeps running,
//! and divide by the per-unit cost of whatever is about to be allocated. The
//! answer scales with the machine and with what else is running on it.
//!
//! ## What "available" means
//!
//! Not total RAM. Total RAM is a hardware fact; what matters is what the
//! kernel would hand over without swapping. On Windows that is
//! `MEMORYSTATUSEX::ullAvailPhys`, on Linux `MemAvailable` from
//! `/proc/meminfo` (which, unlike `MemFree`, accounts for reclaimable page
//! cache). Both are measurements, both move while the program runs, and both
//! are re-read rather than cached forever.
//!
//! ## Dependencies
//!
//! None. `GlobalMemoryStatusEx` is four lines of `extern "system"` and
//! `/proc/meminfo` is a text file. A crate like `sysinfo` would pull in
//! process enumeration, disk and network listing, and a CPU sampler to answer
//! one question we can answer in fifty lines — and it would land on the
//! critical path of every workspace compile.
//!
//! ## When the measurement fails
//!
//! Unsupported platform, unreadable `/proc`, a failing syscall: the budget
//! reports [`Source::Fallback`] and uses a deliberately small fixed figure.
//! The UI is expected to say so. A guessed budget that is too small refuses
//! work the machine could have done; a guessed budget that is too large gets
//! the process killed with the user's unsaved edits inside it. We guess small.

use std::sync::atomic::{AtomicU64, Ordering};

/// Where the numbers in a [`Budget`] came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Read from the operating system.
    Measured,
    /// The user pinned it with `FERRIX_MEM_BUDGET_MB`.
    Override,
    /// The platform could not be asked; [`FALLBACK_AVAILABLE`] is in use.
    Fallback,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Measured => "measured",
            Source::Override => "FERRIX_MEM_BUDGET_MB",
            Source::Fallback => "unmeasured — conservative default",
        }
    }
}

/// Assumed available memory when the platform cannot be asked: 512 MB.
///
/// Small on purpose. See the module docs — under-guessing costs a refusal,
/// over-guessing costs the user's work.
pub const FALLBACK_AVAILABLE: u64 = 512 << 20;

/// Never budget away the last of the machine's memory. Whatever the
/// measurement says, this much is left for the compositor, the shell, and
/// whatever else the user has open.
pub const RESERVE_BYTES: u64 = 512 << 20;

/// Fraction of the remaining available memory a single Ferrix operation may
/// claim. Half, so two large things back to back do not collide, and so the
/// measurement being slightly stale is not fatal.
pub const CLAIM_FRACTION: u64 = 2;

/// The floor on any derived cap, in bytes.
///
/// Even a machine under extreme pressure should be able to copy a screenful
/// of cells. Below this the caps stop being a safety feature and start being
/// a broken application.
pub const MIN_CLAIM: u64 = 16 << 20;

/// A memory measurement plus the caps derived from it.
///
/// Cheap to copy and cheap to take (a syscall or one small file read), so
/// call sites re-sample rather than passing one around for the life of the
/// process — "available" five minutes ago is not a measurement.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// Physical memory installed. 0 when unknown.
    pub total: u64,
    /// What the OS says it could hand out right now.
    pub available: u64,
    pub source: Source,
}

impl Budget {
    /// Measure now.
    pub fn sample() -> Self {
        if let Some(mb) = env_override_mb() {
            return Self {
                total: platform::total_bytes().unwrap_or(0),
                available: mb.saturating_mul(1 << 20),
                source: Source::Override,
            };
        }
        match platform::available_bytes() {
            Some(available) => Self {
                total: platform::total_bytes().unwrap_or(0),
                available,
                source: Source::Measured,
            },
            None => Self {
                total: 0,
                available: FALLBACK_AVAILABLE,
                source: Source::Fallback,
            },
        }
    }

    /// Construct a budget directly. For tests and for callers that have
    /// already measured and want the derivation without a second syscall.
    pub fn from_available(available: u64) -> Self {
        Self {
            total: 0,
            available,
            source: Source::Measured,
        }
    }

    /// The most a single operation may allocate, in bytes.
    ///
    /// `available`, minus the reserve, halved — floored at [`MIN_CLAIM`] so
    /// small operations never become impossible.
    pub fn claim_bytes(&self) -> u64 {
        let usable = self.available.saturating_sub(RESERVE_BYTES);
        (usable / CLAIM_FRACTION).max(MIN_CLAIM)
    }

    /// How many units of `bytes_each` fit inside the claim.
    ///
    /// `bytes_each` of 0 is treated as 1: a caller that mis-estimates a cost
    /// as free must not be handed `u64::MAX` as a cap.
    pub fn max_units(&self, bytes_each: u64) -> u64 {
        self.claim_bytes() / bytes_each.max(1)
    }

    /// Same, saturated into `usize` for indexing callers.
    pub fn max_units_usize(&self, bytes_each: u64) -> usize {
        self.max_units(bytes_each).min(usize::MAX as u64) as usize
    }

    /// Would allocating `bytes` for `what` fit? `Err` carries a message the
    /// UI can show verbatim.
    ///
    /// This is the function that turns an OOM kill into a sentence. Refusing
    /// is always better than being killed: the process holds unsaved edits.
    pub fn admit(&self, bytes: u64, what: &str) -> Result<(), String> {
        let claim = self.claim_bytes();
        if bytes <= claim {
            return Ok(());
        }
        Err(format!(
            "{what} needs {} but only {} is safely available ({} of {} free, {}). \
             Reduce the selection or close something.",
            fmt_bytes(bytes),
            fmt_bytes(claim),
            fmt_bytes(self.available),
            if self.total > 0 {
                fmt_bytes(self.total)
            } else {
                "unknown".to_string()
            },
            self.source.label(),
        ))
    }

    /// True when the measurement is real rather than a fallback guess.
    pub fn is_measured(&self) -> bool {
        matches!(self.source, Source::Measured | Source::Override)
    }

    /// One line for the status bar.
    pub fn describe(&self) -> String {
        if self.total > 0 {
            format!(
                "{} of {} RAM available · {} budget ({})",
                fmt_bytes(self.available),
                fmt_bytes(self.total),
                fmt_bytes(self.claim_bytes()),
                self.source.label()
            )
        } else {
            format!(
                "{} available · {} budget ({})",
                fmt_bytes(self.available),
                fmt_bytes(self.claim_bytes()),
                self.source.label()
            )
        }
    }
}

/// Per-unit cost estimates, used to turn a byte budget into a count cap.
///
/// These are deliberately pessimistic. Under-estimating a cost means the cap
/// admits an allocation that then does not fit, which is the failure the whole
/// module exists to prevent.
pub mod cost {
    /// One clipboard cell: a `String` header plus a short rendered value, in
    /// a `Vec<Vec<String>>` whose inner vectors also carry headers.
    pub const CLIPBOARD_CELL: u64 = 64;

    /// One chart sample held as `Option<f64>` during aggregation, plus slack
    /// for the intermediate point vectors the scene builder allocates.
    pub const CHART_ROW: u64 = 32;

    /// One search hit: a `CellRef` in the results vector, plus the row-filter
    /// mapping built from the same results.
    pub const SEARCH_HIT: u64 = 24;

    /// One overlay cell written by a paste or fill: `HashMap` entry, key,
    /// `CellInput`, and undo bookkeeping (before + after).
    pub const OVERLAY_CELL: u64 = 160;

    /// One cell rewritten by Replace All. Costs an `OVERLAY_CELL` plus the two
    /// replacement strings the before/after pair holds — a replace writes text,
    /// where a clear writes `Value::Empty`. Deliberately generous: the cap
    /// derived from this is what stops a Replace All over 200M rows from
    /// building an undo entry larger than memory.
    pub const REPLACE_CELL: u64 = OVERLAY_CELL + 128;
}

/// A cached process-wide sample, refreshed on demand.
///
/// Sampling costs a syscall, which is nothing next to a large operation but is
/// too much to do inside a per-frame paint loop. `cached()` reuses a sample
/// for [`CACHE_MILLIS`]; anything about to allocate should call
/// [`Budget::sample`] directly.
static CACHED_AVAILABLE: AtomicU64 = AtomicU64::new(0);
static CACHED_TOTAL: AtomicU64 = AtomicU64::new(0);
static CACHED_AT_MILLIS: AtomicU64 = AtomicU64::new(0);
static CACHED_SOURCE: AtomicU64 = AtomicU64::new(u64::MAX);

/// How long a cached sample stays fresh.
pub const CACHE_MILLIS: u64 = 1000;

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A budget no older than [`CACHE_MILLIS`]. Safe to call every frame.
pub fn cached() -> Budget {
    let now = now_millis();
    let at = CACHED_AT_MILLIS.load(Ordering::Relaxed);
    let src = CACHED_SOURCE.load(Ordering::Relaxed);
    if src != u64::MAX && now.saturating_sub(at) < CACHE_MILLIS {
        return Budget {
            total: CACHED_TOTAL.load(Ordering::Relaxed),
            available: CACHED_AVAILABLE.load(Ordering::Relaxed),
            source: match src {
                0 => Source::Measured,
                1 => Source::Override,
                _ => Source::Fallback,
            },
        };
    }
    let b = Budget::sample();
    CACHED_TOTAL.store(b.total, Ordering::Relaxed);
    CACHED_AVAILABLE.store(b.available, Ordering::Relaxed);
    CACHED_SOURCE.store(
        match b.source {
            Source::Measured => 0,
            Source::Override => 1,
            Source::Fallback => 2,
        },
        Ordering::Relaxed,
    );
    CACHED_AT_MILLIS.store(now, Ordering::Relaxed);
    b
}

fn env_override_mb() -> Option<u64> {
    std::env::var("FERRIX_MEM_BUDGET_MB")
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|&v| v > 0)
}

/// Human-readable byte count.
pub fn fmt_bytes(b: u64) -> String {
    const KB: f64 = 1024.0;
    let f = b as f64;
    if f < KB {
        format!("{b} B")
    } else if f < KB * KB {
        format!("{:.0} KB", f / KB)
    } else if f < KB * KB * KB {
        format!("{:.1} MB", f / (KB * KB))
    } else {
        format!("{:.2} GB", f / (KB * KB * KB))
    }
}

// --- platform measurement --------------------------------------------------

#[cfg(target_os = "windows")]
mod platform {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    // The whole Windows dependency: one function from kernel32, which every
    // Rust binary already links against.
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    fn status() -> Option<MemoryStatusEx> {
        let mut s = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `s` is a correctly sized, correctly initialized
        // MEMORYSTATUSEX with `length` set as the API requires. The call only
        // writes into it.
        let ok = unsafe { GlobalMemoryStatusEx(&mut s) };
        (ok != 0).then_some(s)
    }

    pub fn available_bytes() -> Option<u64> {
        status().map(|s| s.avail_phys)
    }

    pub fn total_bytes() -> Option<u64> {
        status().map(|s| s.total_phys)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// Read one `/proc/meminfo` key, in bytes. Values there are in kB.
    fn meminfo(key: &str) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in text.lines() {
            let Some(rest) = line.strip_prefix(key) else {
                continue;
            };
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
        None
    }

    pub fn available_bytes() -> Option<u64> {
        // MemAvailable, not MemFree: page cache is reclaimable, and treating
        // it as unavailable would refuse work on any machine that has read a
        // file recently — which is every machine.
        meminfo("MemAvailable").or_else(|| meminfo("MemFree"))
    }

    pub fn total_bytes() -> Option<u64> {
        meminfo("MemTotal")
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod platform {
    // macOS and the BSDs need `sysctl`/`host_statistics64`, which is more
    // unsafe FFI than this crate should carry unverified. Reporting "unknown"
    // and taking the conservative fallback is honest; a wrong number here
    // would be worse than no number.
    pub fn available_bytes() -> Option<u64> {
        None
    }
    pub fn total_bytes() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_measured_budget_is_plausible() {
        let b = Budget::sample();
        // On the platforms we can measure, available must be non-zero and no
        // larger than total. On others we must be honestly reporting the
        // fallback rather than inventing a figure.
        if b.is_measured() {
            assert!(b.available > 0, "measured budget reported zero available");
            if b.total > 0 {
                assert!(b.available <= b.total, "available exceeds total");
            }
        } else {
            assert_eq!(b.available, FALLBACK_AVAILABLE);
        }
    }

    #[test]
    fn caps_scale_with_the_machine() {
        // The entire point of the module: a bigger machine gets a bigger cap.
        let small = Budget::from_available(2 << 30);
        let large = Budget::from_available(64 << 30);
        assert!(
            large.max_units(cost::CLIPBOARD_CELL) > small.max_units(cost::CLIPBOARD_CELL) * 8,
            "caps must track measured memory, not a constant"
        );
    }

    #[test]
    fn a_starved_machine_still_gets_a_usable_floor() {
        // Available below the reserve must not produce a zero cap, or the app
        // becomes unusable rather than merely careful.
        let starved = Budget::from_available(64 << 20);
        assert_eq!(starved.claim_bytes(), MIN_CLAIM);
        assert!(starved.max_units(cost::CLIPBOARD_CELL) > 100_000);
    }

    #[test]
    fn the_reserve_is_actually_held_back() {
        let b = Budget::from_available(8 << 30);
        assert!(
            b.claim_bytes() < 8 << 30,
            "budget must not claim all of available memory"
        );
        assert_eq!(b.claim_bytes(), ((8u64 << 30) - RESERVE_BYTES) / 2);
    }

    #[test]
    fn zero_cost_units_do_not_produce_an_infinite_cap() {
        // A caller that estimates a per-unit cost of zero must not be handed
        // u64::MAX and told to allocate it.
        let b = Budget::from_available(4 << 30);
        assert_eq!(b.max_units(0), b.claim_bytes());
    }

    #[test]
    fn admit_refuses_with_an_actionable_message() {
        let b = Budget::from_available(2 << 30);
        let err = b.admit(64 << 30, "This copy").unwrap_err();
        assert!(err.starts_with("This copy needs"), "{err}");
        assert!(err.contains("available"), "{err}");
        // The message must name a remedy, not just a failure.
        assert!(err.contains("Reduce the selection"), "{err}");
    }

    #[test]
    fn admit_allows_what_fits() {
        let b = Budget::from_available(8 << 30);
        assert!(b.admit(1 << 20, "small").is_ok());
    }

    #[test]
    fn cached_is_stable_within_its_window() {
        let a = cached();
        let b = cached();
        assert_eq!(
            a.available, b.available,
            "cache must not re-sample per call"
        );
    }

    #[test]
    fn byte_formatting_is_readable() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2 << 20), "2.0 MB");
        assert_eq!(fmt_bytes(3 << 30), "3.00 GB");
    }
}
