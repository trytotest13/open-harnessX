//! Process supervision helpers (Windows: detached console-less children,
//! tree kill via taskkill).

use std::path::Path;
use std::process::{Child, Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Spawn `cmd` with stdout+stderr redirected to `log_path`, no console window.
/// The child outlives this process (no job object is attached).
pub fn spawn_logged(cmd: &mut Command, log_path: &Path) -> std::io::Result<Child> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_path)?;
    let err = log.try_clone()?;
    cmd.stdout(Stdio::from(log)).stderr(Stdio::from(err));
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.spawn()
}

/// Kill a process and its whole tree.
pub fn kill_tree(pid: u32) -> Result<(), String> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()
        .map_err(|e| e.to_string())?;
    if status.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&status.stderr).trim().to_string())
    }
}

/// Hard-cap a process's resident RAM (the working set Task Manager shows).
/// A model that genuinely needs more will slow down under page-fault
/// pressure rather than eating the machine; the size gate in reload_engine
/// is the first line of defense, this is the backstop.
#[cfg(windows)]
pub fn limit_working_set(pid: u32, bytes: usize) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn SetProcessWorkingSetSizeEx(h: isize, min: usize, max: usize, flags: u32) -> i32;
        fn CloseHandle(h: isize) -> i32;
    }
    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const QUOTA_LIMITS_HARD_WS_MAX_ENABLE: u32 = 0x0000_0002;
    unsafe {
        let h = OpenProcess(PROCESS_SET_QUOTA, 0, pid);
        if h == 0 {
            return false;
        }
        let ok = SetProcessWorkingSetSizeEx(
            h,
            20 * 1024 * 1024,
            bytes,
            QUOTA_LIMITS_HARD_WS_MAX_ENABLE,
        );
        CloseHandle(h);
        ok != 0
    }
}

#[cfg(not(windows))]
pub fn limit_working_set(_pid: u32, _bytes: usize) -> bool {
    false
}
