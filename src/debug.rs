//! Traces for `--verbose`: what was sent and what came back, on the error output.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Writes a trace line; the text is only built when traces are on.
pub fn log(line: impl FnOnce() -> String) {
    if ENABLED.load(Ordering::Relaxed) {
        eprintln!("{}", line());
    }
}
