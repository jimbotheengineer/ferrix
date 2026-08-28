//! Does a panic in a worker thread take the whole process down?
//!
//! The app loads files on a background thread and handles a dead loader
//! gracefully (`TryRecvError::Disconnected` -> "Load thread died"). That
//! recovery only works if a panicking thread *unwinds* and drops its end of
//! the channel. Under `panic = "abort"` the whole process dies instead,
//! taking unsaved edits with it and skipping the unsaved-changes prompt.
//!
//! Run under the release profile to see which behaviour actually ships.

use std::sync::mpsc;

fn main() {
    println!("panic strategy probe");

    let (tx, rx) = mpsc::channel::<u32>();

    let h = std::thread::spawn(move || {
        // Hold the sender so dropping it on unwind disconnects the channel,
        // exactly as the real loader thread does.
        let _tx = tx;
        panic!("simulated loader failure");
    });

    // If the process survives to here, the panic unwound.
    match rx.recv() {
        Ok(v) => println!("UNEXPECTED: received {v}"),
        Err(_) => println!("channel disconnected -> main thread SURVIVED the worker panic"),
    }

    let joined = h.join();
    println!("join result is_err = {}", joined.is_err());
    println!("RESULT: unwind (recoverable)");
}
