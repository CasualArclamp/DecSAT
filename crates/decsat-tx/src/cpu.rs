// made by CasualArclamp
//! CPU meters (100 % is one core): this process, and a child process
//! (ffmpeg) by its handle. Copied from DecDVB-T's `decdvbt-tx/src/cpu.rs`,
//! with the child's added.

use std::time::Instant;

#[cfg(windows)]
fn times(handle: windows_sys::Win32::Foundation::HANDLE) -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: `handle` is a live process handle (ours, or a child's that the
    // caller holds open), and the four pointers are to live locals the call
    // fills in.
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    if ok == 0 {
        return 0.0;
    }
    let ticks = |t: FILETIME| (((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64) as f64;
    (ticks(kernel) + ticks(user)) * 1e-7
}

/// CPU seconds (user + kernel) this process has used so far.
#[cfg(windows)]
pub fn process_cpu_seconds() -> f64 {
    // SAFETY: a pseudo-handle that needs no closing.
    times(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
}

/// CPU seconds a child process has used so far.
#[cfg(windows)]
pub fn child_cpu_seconds(child: &std::process::Child) -> f64 {
    // Rust: `AsRawHandle` gives the OS handle std keeps open for the child.
    use std::os::windows::io::AsRawHandle;
    times(child.as_raw_handle() as _)
}

#[cfg(not(windows))]
fn proc_stat(path: &str) -> f64 {
    let Ok(stat) = std::fs::read_to_string(path) else {
        return 0.0;
    };
    let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
        return 0.0;
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (tick(11) + tick(12)) / 100.0
}

#[cfg(not(windows))]
pub fn process_cpu_seconds() -> f64 {
    proc_stat("/proc/self/stat")
}

#[cfg(not(windows))]
pub fn child_cpu_seconds(child: &std::process::Child) -> f64 {
    proc_stat(&format!("/proc/{}/stat", child.id()))
}

/// Turns a running CPU-seconds counter into a percentage over each
/// interval of a second or more.
pub struct CpuGauge {
    last_cpu: f64,
    last_at: Instant,
    pct: f32,
}

impl CpuGauge {
    pub fn new(now_cpu: f64) -> CpuGauge {
        CpuGauge {
            last_cpu: now_cpu,
            last_at: Instant::now(),
            pct: 0.0,
        }
    }

    /// Give the counter's value now; the reading is renewed about once a
    /// second.
    pub fn update(&mut self, cpu: f64) -> f32 {
        let dt = self.last_at.elapsed().as_secs_f64();
        if dt >= 1.0 {
            self.pct = (100.0 * (cpu - self.last_cpu) / dt) as f32;
            self.last_cpu = cpu;
            self.last_at = Instant::now();
        }
        self.pct
    }
}
