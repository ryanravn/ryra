use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set(enabled: bool) {
    VERBOSE.store(enabled, Ordering::Relaxed);
}

pub fn is_enabled() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}
