//! Win32 thread scheduling hints. No CPU affinity and no extra crates.
//! The receiver must not use REALTIME_PRIORITY_CLASS or THREAD_PRIORITY_TIME_CRITICAL.
#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut c_void;
        fn SetThreadPriority(h_thread: *mut c_void, priority: i32) -> i32;
    }
    pub(super) fn set(priority: i32) {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), priority);
        }
    }
}

pub(crate) fn set_dsp_priority() {
    #[cfg(windows)]
    win::set(1); // THREAD_PRIORITY_ABOVE_NORMAL
}
pub(crate) fn set_display_priority() {
    #[cfg(windows)]
    win::set(-1); // THREAD_PRIORITY_BELOW_NORMAL
}
