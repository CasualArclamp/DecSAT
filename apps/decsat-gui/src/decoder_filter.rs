// made by CasualArclamp
//! Which decoders the VFO menus offer. On a smart-mode server only the ones
//! that run remotely are listed. The menus are drawn deep in the side panel,
//! far from the app's state, so the app sets this once a frame.

use std::sync::atomic::{AtomicBool, Ordering};

use decsat_engine::DecoderKind;

static REMOTE_ONLY: AtomicBool = AtomicBool::new(false);

pub fn set(remote_only: bool) {
    REMOTE_ONLY.store(remote_only, Ordering::Relaxed);
}

pub fn allowed(k: DecoderKind) -> bool {
    !REMOTE_ONLY.load(Ordering::Relaxed) || decsat_remote::served(k)
}
