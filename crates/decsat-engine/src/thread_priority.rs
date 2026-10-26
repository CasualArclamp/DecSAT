//! Win32 thread scheduling hints. No CPU affinity and no extra crates.
//!
//! The receiver must not use REALTIME_PRIORITY_CLASS or THREAD_PRIORITY_TIME_CRITICAL.
//! Only the thread's priority *within* the normal process class moves, by one
//! step either way: the DSP workers go a notch up so they keep up with the
//! radio, the display thread a notch down so a slow spectrum FFT yields to
//! them. Other platforms leave scheduling to the OS (both functions do nothing).
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25).

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    // Rust note: since edition 2024 an `extern` block must be written
    // `unsafe extern`, because the compiler cannot check that the declared
    // signatures match the real functions. `"system"` is the platform's system
    // calling convention (stdcall on 32-bit Windows, the usual one on 64-bit).
    #[link(name = "kernel32")]
    unsafe extern "system" {
        /// A pseudo-handle for the calling thread: always valid for that
        /// thread, and never needs closing.
        fn GetCurrentThread() -> *mut c_void;
        /// Returns non-zero on success.
        fn SetThreadPriority(h_thread: *mut c_void, priority: i32) -> i32;
    }

    /// Set the calling thread's priority to one of the `THREAD_PRIORITY_*`
    /// levels from `processthreadsapi.h`. Best effort: a refusal is ignored,
    /// as the thread still runs at its old priority.
    pub(super) fn set(priority: i32) {
        // SAFETY: `GetCurrentThread` returns a valid pseudo-handle for this
        // thread, and `SetThreadPriority` only reads its two arguments.
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), priority);
        }
    }
}

/// `THREAD_PRIORITY_ABOVE_NORMAL` (`processthreadsapi.h`).
#[cfg(windows)]
const THREAD_PRIORITY_ABOVE_NORMAL: i32 = 1;
/// `THREAD_PRIORITY_BELOW_NORMAL` (`processthreadsapi.h`).
#[cfg(windows)]
const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;

/// Raise the calling thread (a VFO's DSP worker) one step above normal.
pub(crate) fn set_dsp_priority() {
    #[cfg(windows)]
    win::set(THREAD_PRIORITY_ABOVE_NORMAL);
}

/// Lower the calling thread (the spectrum display thread) one step below normal.
pub(crate) fn set_display_priority() {
    #[cfg(windows)]
    win::set(THREAD_PRIORITY_BELOW_NORMAL);
}
