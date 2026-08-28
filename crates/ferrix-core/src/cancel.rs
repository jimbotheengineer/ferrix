//! Cooperative cancellation.
//!
//! Long operations in Ferrix run on worker threads: converting a 10 GB CSV,
//! exporting 200M rows. Both take minutes, and a user who started one by
//! mistake needs a way out that is not "kill the process and lose the edits".
//!
//! There is no way to safely stop a thread from outside, so cancellation is
//! cooperative: the UI flips a flag, the worker polls it at a known cadence
//! and unwinds cleanly, deleting its partial output. The cadence is what makes
//! this real rather than decorative — a token polled once per file is a lie.
//! Every consumer here polls at most a few hundred milliseconds of work apart.
//!
//! [`ferrix_io::export::export_csv`] already took a `should_cancel` closure;
//! this is that pattern given a name and a shared handle so the flag can live
//! on the UI side of the thread boundary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared "stop what you are doing" flag.
///
/// Clone it: the UI keeps one handle, the worker takes the other. Setting it
/// from either side is visible to both.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the holder to stop. Idempotent; safe from any thread.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Has cancellation been requested?
    ///
    /// `Relaxed` is correct here: this is a one-way latch whose only effect is
    /// to make a worker return early. Nothing is published through it, so no
    /// ordering is needed — and a poll in a hot loop should not cost a fence.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// A closure for APIs that take `FnMut() -> bool`.
    pub fn checker(&self) -> impl FnMut() -> bool + Send + 'static {
        let flag = Arc::clone(&self.flag);
        move || flag.load(Ordering::Relaxed)
    }

    /// Clear the flag so the token can be reused for the next operation.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_uncancelled() {
        assert!(!CancelToken::new().is_cancelled());
    }

    #[test]
    fn cancellation_is_visible_through_a_clone() {
        // The whole contract: the UI's handle and the worker's handle are the
        // same flag.
        let ui = CancelToken::new();
        let worker = ui.clone();
        assert!(!worker.is_cancelled());
        ui.cancel();
        assert!(worker.is_cancelled());
    }

    #[test]
    fn cancellation_crosses_a_real_thread_boundary() {
        let ui = CancelToken::new();
        let worker = ui.clone();
        let handle = std::thread::spawn(move || {
            let mut spun = 0u64;
            while !worker.is_cancelled() {
                spun += 1;
                if spun > 5_000_000_000 {
                    return Err("token never observed as cancelled");
                }
                std::hint::spin_loop();
            }
            Ok(spun)
        });
        // Give the worker a moment to actually be spinning, then stop it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        ui.cancel();
        assert!(
            handle.join().unwrap().is_ok(),
            "worker did not observe cancel"
        );
    }

    #[test]
    fn checker_closure_tracks_the_token() {
        let t = CancelToken::new();
        let mut check = t.checker();
        assert!(!check());
        t.cancel();
        assert!(check(), "checker must see cancellation after it is issued");
    }

    #[test]
    fn reset_allows_reuse() {
        let t = CancelToken::new();
        t.cancel();
        assert!(t.is_cancelled());
        t.reset();
        assert!(!t.is_cancelled(), "a reset token must be usable again");
    }
}
