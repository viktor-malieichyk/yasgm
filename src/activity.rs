//! What the sync in progress is doing right now ("waiting" for another
//! operation to finish, "checking", "uploading", "downloading"), set by the
//! sync engine and read by the daemon's `status` (the Decky game page shows
//! it next to its progress bar).

use std::sync::Mutex;

static PHASE: Mutex<&str> = Mutex::new("");

pub fn set(phase: &'static str) {
    *PHASE.lock().unwrap_or_else(|p| p.into_inner()) = phase;
}

pub fn get() -> &'static str {
    *PHASE.lock().unwrap_or_else(|p| p.into_inner())
}
