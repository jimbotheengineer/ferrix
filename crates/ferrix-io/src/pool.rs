//! Worker pool sizing.
//!
//! ## The problem
//!
//! Converting a 10 GB CSV is embarrassingly parallel, so rayon's default —
//! one worker per logical core — is the fastest possible choice for the
//! conversion considered alone. Considered alongside the human sitting in
//! front of the machine, it is the wrong one. With every core saturated the
//! compositor misses frames, the editor stutters, and Ferrix's own UI thread
//! competes with sixteen workers for a scheduler slot. The application becomes
//! the reason the computer is unusable for the several minutes it is working.
//!
//! ## The policy
//!
//! Leave at least one core free. `available_parallelism() - 1`, floored at 1
//! so a single-core machine still runs. That core is what the UI thread, the
//! window manager, and the user's other work run on.
//!
//! This is not free — see `RESOURCES.md` for the measured trade on this
//! machine. The default is n-1 because the throughput given up is small
//! relative to the responsiveness bought, and because a conversion the user
//! cancels out of frustration has a throughput of zero.
//!
//! ## Configuring it
//!
//! `FERRIX_THREADS` overrides the count. `FERRIX_THREADS=0` means "all cores"
//! for someone running a batch conversion on a machine they are not sitting
//! at; any other positive value is used verbatim, clamped to the core count.
//!
//! ## Why a global pool and not a scoped one
//!
//! `build_global` is called once, early, from [`init`]. Every `par_iter` in
//! the crate then lands in the configured pool without any call site needing
//! to know. Installing a private pool per operation would mean threading a
//! pool handle through every parse function for no behavioural difference.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The thread count actually in force, or 0 before [`init`] runs.
static CONFIGURED: AtomicUsize = AtomicUsize::new(0);

/// Logical cores, or 1 if the platform will not say.
pub fn core_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// How many workers Ferrix should use by default: all cores but one.
pub fn default_threads() -> usize {
    core_count().saturating_sub(1).max(1)
}

/// The count [`init`] will apply, taking `FERRIX_THREADS` into account.
pub fn configured_threads() -> usize {
    match std::env::var("FERRIX_THREADS").ok().and_then(|v| {
        let t = v.trim();
        if t == "0" {
            // Explicit opt-in to saturating the machine.
            Some(core_count())
        } else {
            t.parse::<usize>().ok().filter(|&n| n > 0)
        }
    }) {
        Some(n) => n.min(core_count()),
        None => default_threads(),
    }
}

/// Size the global rayon pool. Idempotent and safe to call from anywhere.
///
/// Returns the thread count in force. Rayon only permits one `build_global`
/// per process; a second call (or a pool rayon already built lazily) fails,
/// and the honest answer then is whatever rayon is actually running, not what
/// we asked for.
pub fn init() -> usize {
    let want = configured_threads();
    let actual = match rayon::ThreadPoolBuilder::new()
        .num_threads(want)
        .thread_name(|i| format!("ferrix-worker-{i}"))
        .build_global()
    {
        Ok(()) => want,
        // Already initialized — report the truth rather than the intent.
        Err(_) => rayon::current_num_threads(),
    };
    CONFIGURED.store(actual, Ordering::Relaxed);
    actual
}

/// One line describing the pool, for the status bar.
pub fn describe() -> String {
    let n = match CONFIGURED.load(Ordering::Relaxed) {
        0 => rayon::current_num_threads(),
        n => n,
    };
    let cores = core_count();
    if n < cores {
        format!("{n} of {cores} cores (leaving {} free)", cores - n)
    } else {
        format!("{n} of {cores} cores (all)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_leaves_a_core_free() {
        // The acceptance criterion, as a unit test.
        let cores = core_count();
        let t = default_threads();
        if cores > 1 {
            assert_eq!(t, cores - 1, "default must leave exactly one core free");
            assert!(t >= 1);
        } else {
            assert_eq!(t, 1, "a single-core machine must still get a worker");
        }
    }

    #[test]
    fn never_zero_threads() {
        assert!(default_threads() >= 1);
        assert!(configured_threads() >= 1);
    }

    #[test]
    fn configured_never_exceeds_the_machine() {
        // A user typing FERRIX_THREADS=9999 should not get 9999 threads.
        assert!(configured_threads() <= core_count());
    }

    #[test]
    fn describe_names_the_free_core() {
        let d = describe();
        assert!(d.contains("cores"), "{d}");
    }
}
