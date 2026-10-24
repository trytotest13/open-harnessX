//! Machine + engine telemetry: system RAM, GPU VRAM (nvidia-smi), and the
//! engine process's own memory. This is the data behind the dashboard's
//! multitier view — how much of the model lives in VRAM, in RAM, or is still
//! streamed from disk — Colibri's "one hierarchy" idea, measured with the
//! tools Windows actually gives us. All calls are cheap, cached, and fail
//! soft: a missing GPU or an unopenable process degrades one number, never
//! the whole report.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Total and available physical RAM, in bytes.
pub struct SysRam {
    pub total: u64,
    pub avail: u64,
}

/// One GPU as reported by nvidia-smi.
#[derive(Clone)]
pub struct Gpu {
    pub name: String,
    /// MiB.
    pub total: u64,
    /// MiB in use by every process on the machine.
    pub used: u64,
}

#[cfg(windows)]
mod win {
    #[repr(C)]
    pub struct MemoryStatusEx {
        pub dw_length: u32,
        pub dw_memory_load: u32,
        pub ull_total_phys: u64,
        pub ull_avail_phys: u64,
        pub ull_total_page_file: u64,
        pub ull_avail_page_file: u64,
        pub ull_total_virtual: u64,
        pub ull_avail_virtual: u64,
        pub ull_avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    #[repr(C)]
    pub struct ProcessMemoryCounters {
        pub cb: u32,
        pub page_fault_count: u32,
        pub peak_working_set: usize,
        pub working_set_size: usize,
        pub quota_peak_paged_pool_usage: usize,
        pub quota_paged_pool_usage: usize,
        pub quota_non_paged_pool_usage: usize,
        pub page_file_usage: usize,
        pub peak_page_file_usage: usize,
        /// PROCESS_MEMORY_COUNTERS_EX tail: recent Windows builds validate
        /// against the EX size and reject the plain 64-byte struct with
        /// ERROR_INSUFFICIENT_BUFFER (measured on build 26300).
        pub private_usage: usize,
    }

    #[link(name = "psapi")]
    extern "system" {
        fn GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
    }

    pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    pub const PROCESS_QUERY_INFORMATION: u32 = 0x0400;

    pub fn sys_ram() -> Option<(u64, u64)> {
        let mut st = MemoryStatusEx {
            dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
            dw_memory_load: 0,
            ull_total_phys: 0,
            ull_avail_phys: 0,
            ull_total_page_file: 0,
            ull_avail_page_file: 0,
            ull_total_virtual: 0,
            ull_avail_virtual: 0,
            ull_avail_extended_virtual: 0,
        };
        unsafe {
            (GlobalMemoryStatusEx(&mut st) != 0).then(|| (st.ull_total_phys, st.ull_avail_phys))
        }
    }

    /// Resident working set of one process, bytes (None = can't tell).
    /// Limited-information access first — full query access can be denied
    /// even for same-user processes; GetProcessMemoryInfo accepts both.
    pub fn working_set(pid: u32) -> Option<u64> {
        unsafe {
            let mut h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h == 0 {
                h = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
            }
            if h == 0 {
                return None;
            }
            let mut pmc = ProcessMemoryCounters {
                cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
                page_fault_count: 0,
                peak_working_set: 0,
                working_set_size: 0,
                quota_peak_paged_pool_usage: 0,
                quota_paged_pool_usage: 0,
                quota_non_paged_pool_usage: 0,
                page_file_usage: 0,
                peak_page_file_usage: 0,
                private_usage: 0,
            };
            let ok = GetProcessMemoryInfo(
                h,
                &mut pmc,
                std::mem::size_of::<ProcessMemoryCounters>() as u32,
            );
            CloseHandle(h);
            (ok != 0).then(|| pmc.working_set_size as u64)
        }
    }
}

#[cfg(not(windows))]
mod win {
    pub fn sys_ram() -> Option<(u64, u64)> {
        None
    }
    pub fn working_set(_pid: u32) -> Option<u64> {
        None
    }
}

pub fn sys_ram() -> Option<SysRam> {
    win::sys_ram().map(|(total, avail)| SysRam { total, avail })
}

/// Resident RAM (working set) of one process in bytes.
pub fn working_set(pid: u32) -> Option<u64> {
    win::working_set(pid)
}

fn nvidia_smi(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("nvidia-smi")
        .args(args)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// GPUs on this machine. Cached for 5 s — the dashboard polls every 2 s and
/// spawning nvidia-smi that often is rude.
pub fn gpus() -> Vec<Gpu> {
    static CACHE: Mutex<Option<(Instant, Vec<Gpu>)>> = Mutex::new(None);
    if let Some((at, gpus)) = CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(5) {
            return gpus.clone();
        }
    }
    let gpus = gpus_uncached();
    *CACHE.lock().unwrap() = Some((Instant::now(), gpus.clone()));
    gpus
}

fn gpus_uncached() -> Vec<Gpu> {
    let Some(text) = nvidia_smi(&[
        "--query-gpu=name,memory.total,memory.used",
        "--format=csv,noheader,nounits",
    ]) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            let name = it.next()?.trim().to_string();
            let total: u64 = it.next()?.trim().parse().ok()?;
            let used: u64 = it.next()?.trim().parse().ok()?;
            (!name.is_empty()).then(|| Gpu { name, total, used })
        })
        .collect()
}

/// VRAM a specific PID holds, in bytes (0 = none measurable).
///
/// WDDM laptops don't expose per-process VRAM to nvidia-smi (`[N/A]` rows),
/// so the primary source is the Windows "GPU Engine" performance counter —
/// the same per-process Dedicated Usage Task Manager shows — with nvidia-smi
/// compute-apps rows (which do carry numbers on TCC/older drivers) as the
/// fallback. The result is cached for 3 s (dashboard poll rate).
pub fn gpu_vram_bytes_of(pid: u32) -> u64 {
    static CACHE: Mutex<Option<(Instant, u32, u64)>> = Mutex::new(None);
    if let Some((at, p, v)) = CACHE.lock().unwrap().as_ref() {
        if *p == pid && at.elapsed() < Duration::from_secs(3) {
            return *v;
        }
    }
    // A machine without the Dedicated Usage counter never gains it mid-run;
    // remember the miss for a minute so dashboard polling doesn't respawn
    // PowerShell every 3 s just to fail again.
    static COUNTER_DEAD_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
    let counter_dead = COUNTER_DEAD_UNTIL
        .lock()
        .unwrap()
        .is_some_and(|t| t.elapsed() < Duration::from_secs(60));
    let v = if counter_dead {
        0
    } else {
        match gpu_engine_counter_bytes(pid) {
            Some(v) if v > 0 => {
                *COUNTER_DEAD_UNTIL.lock().unwrap() = None;
                v
            }
            _ => {
                *COUNTER_DEAD_UNTIL.lock().unwrap() = Some(Instant::now());
                0
            }
        }
    };
    let v = if v > 0 { v } else { nvidia_smi_fallback_bytes(pid) };
    *CACHE.lock().unwrap() = Some((Instant::now(), pid, v));
    v
}

/// Fallback: nvidia-smi per-process rows; skip the [N/A] rows WDDM emits.
fn nvidia_smi_fallback_bytes(pid: u32) -> u64 {
    let Some(text) = nvidia_smi(&[
        "--query-compute-apps=pid,used_memory",
        "--format=csv,noheader,nounits",
    ]) else {
        return 0;
    };
    text.lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            let p: u32 = it.next()?.trim().parse().ok()?;
            let mib: u64 = it.next()?.trim().parse().ok()?;
            (p == pid).then_some(mib)
        })
        .sum::<u64>()
        * 1024
        * 1024
}

/// Sum of "Dedicated Usage" over every GPU Engine instance of `pid`, bytes.
/// One process can own several engine instances (compute + copy + graphics).
fn gpu_engine_counter_bytes(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-Counter '\\GPU Engine(pid_{pid}*)\\Dedicated Usage' -ErrorAction Stop).CounterSamples | ForEach-Object {{ $_.CookedValue }}"
            ),
        ])
        .creation_flags_compat()
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<f64>().ok())
            .map(|v| v as u64)
            .sum()
    })
}

trait CreationFlagsCompat {
    fn creation_flags_compat(&mut self) -> &mut Self;
}

#[cfg(windows)]
impl CreationFlagsCompat for std::process::Command {
    fn creation_flags_compat(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
    }
}

#[cfg(not(windows))]
impl CreationFlagsCompat for std::process::Command {
    fn creation_flags_compat(&mut self) -> &mut Self {
        self
    }
}
