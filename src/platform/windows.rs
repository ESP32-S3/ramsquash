//! Windows backend: working-set trimming.
//!
//! `EmptyWorkingSet` is the direct analogue of Linux's `memory.reclaim` - it
//! pushes a process's pages out to the pagefile immediately. A hard working-set
//! *maximum* via `SetProcessWorkingSetSizeEx` then acts as the ceiling, and a
//! hard working-set *minimum* is the pin that keeps an app resident.
//!
//! Windows has no per-process PSI, so `stall_pct` is always 0 here and the
//! engine falls back to page-fault and CPU deltas to notice an app being used.
//! Processes owned by another user, or elevated ones, need matching privileges;
//! those failures are reported rather than hidden.

use super::{AppStats, Backend, SquashOutcome, Target};
use anyhow::{Result, anyhow, bail};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::ProcessStatus::{
    EmptyWorkingSet, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
// Set/GetProcessWorkingSetSizeEx live under System::Memory in windows-sys,
// not System::Threading where the rest of the process API sits.
use windows_sys::Win32::System::Memory::{GetProcessWorkingSetSizeEx, SetProcessWorkingSetSizeEx};
use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess};

// Declared locally rather than imported so this file does not depend on which
// windows-sys feature happens to re-export them.
const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
const PROCESS_SET_QUOTA: u32 = 0x0100;
const PROCESS_VM_READ: u32 = 0x0010;

const QUOTA_LIMITS_HARDWS_MIN_ENABLE: u32 = 0x0000_0001;
const QUOTA_LIMITS_HARDWS_MIN_DISABLE: u32 = 0x0000_0002;
const QUOTA_LIMITS_HARDWS_MAX_ENABLE: u32 = 0x0000_0004;
const QUOTA_LIMITS_HARDWS_MAX_DISABLE: u32 = 0x0000_0008;

/// A process handle that closes itself.
struct Proc(HANDLE);

impl Proc {
    fn open(pid: u32) -> Result<Self> {
        let access = PROCESS_QUERY_INFORMATION | PROCESS_SET_QUOTA | PROCESS_VM_READ;
        // SAFETY: plain FFI call; the handle is checked for null and owned by Self.
        let h = unsafe { OpenProcess(access, 0, pid) };
        if h.is_null() {
            bail!(
                "cannot open pid {pid} (code {}). Elevated processes and other \
                 users' processes need matching privileges.",
                std::io::Error::last_os_error()
            );
        }
        Ok(Self(h))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // SAFETY: self.0 is a handle we opened and have not closed.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn counters(p: &Proc) -> Result<PROCESS_MEMORY_COUNTERS_EX> {
    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: GetProcessMemoryInfo writes at most `cb` bytes into `c`; the EX
    // layout is a superset of PROCESS_MEMORY_COUNTERS, which is what it takes.
    let ok = unsafe {
        GetProcessMemoryInfo(
            p.raw(),
            &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            c.cb,
        )
    };
    if ok == 0 {
        return Err(anyhow!(
            "GetProcessMemoryInfo failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(c)
}

fn filetime_to_usec(f: FILETIME) -> u64 {
    let ticks = ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    ticks / 10 // 100ns ticks -> microseconds
}

fn cpu_usec(p: &Proc) -> u64 {
    let mut create: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: all four out-params are valid, writable FILETIMEs.
    let ok = unsafe { GetProcessTimes(p.raw(), &mut create, &mut exit, &mut kernel, &mut user) };
    if ok == 0 {
        return 0;
    }
    filetime_to_usec(kernel) + filetime_to_usec(user)
}

fn set_ws(p: &Proc, min: usize, max: usize, flags: u32) -> Result<()> {
    // SAFETY: plain FFI call on a handle opened with PROCESS_SET_QUOTA.
    let ok = unsafe { SetProcessWorkingSetSizeEx(p.raw(), min, max, flags) };
    if ok == 0 {
        return Err(anyhow!(
            "SetProcessWorkingSetSizeEx failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn get_ws(p: &Proc) -> Result<(usize, usize, u32)> {
    let mut min: usize = 0;
    let mut max: usize = 0;
    let mut flags: u32 = 0;
    // SAFETY: three valid out-params.
    let ok = unsafe { GetProcessWorkingSetSizeEx(p.raw(), &mut min, &mut max, &mut flags) };
    if ok == 0 {
        return Err(anyhow!(
            "GetProcessWorkingSetSizeEx failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((min, max, flags))
}

pub struct WindowsBackend;

impl WindowsBackend {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

fn pid_of(t: &Target) -> Result<u32> {
    t.key
        .parse()
        .map_err(|_| anyhow!("malformed windows target key {:?}", t.key))
}

impl Backend for WindowsBackend {
    fn id(&self) -> &'static str {
        "windows-workingset"
    }

    fn mechanism(&self) -> String {
        "EmptyWorkingSet to evict, a hard working-set maximum as the ceiling, \
         a hard minimum to pin"
            .into()
    }

    fn supports_stall(&self) -> bool {
        false // no per-process PSI on Windows
    }

    /// One target per process: Windows limits are per-process, not per-group.
    fn targets(&self, pids: &[u32]) -> Vec<Target> {
        pids.iter()
            .filter(|&&pid| pid != std::process::id())
            .map(|&pid| Target {
                key: pid.to_string(),
                label: format!("pid {pid}"),
                pids: vec![pid],
            })
            .collect()
    }

    fn stats(&self, t: &Target) -> Option<AppStats> {
        let p = Proc::open(pid_of(t).ok()?).ok()?;
        let c = counters(&p).ok()?;
        Some(AppStats {
            rss: c.WorkingSetSize as u64,
            // Private committed bytes that are not resident: an estimate of
            // what currently sits in the pagefile. Windows exposes no exact
            // per-process figure.
            swap: (c.PrivateUsage as u64).saturating_sub(c.WorkingSetSize as u64),
            // Hard faults stand in for cgroup throttle events: both rise when
            // an app is pulling its working set back in.
            throttle_events: c.PageFaultCount as u64,
            cpu_usec: cpu_usec(&p),
            stall_pct: 0.0,
        })
    }

    fn squash(&self, t: &Target, headroom: u64, floor: u64) -> Result<SquashOutcome> {
        let pid = pid_of(t)?;
        if pid == std::process::id() {
            bail!("refusing to squash ourselves");
        }
        let p = Proc::open(pid)?;
        let before = counters(&p)?.WorkingSetSize as u64;

        // SAFETY: handle opened with PROCESS_SET_QUOTA, which this requires.
        let ok = unsafe { EmptyWorkingSet(p.raw()) };
        if ok == 0 {
            return Err(anyhow!(
                "EmptyWorkingSet failed for pid {pid}: {}",
                std::io::Error::last_os_error()
            ));
        }

        let after = counters(&p)?.WorkingSetSize as u64;
        let cap = (after + headroom).max(floor);
        // A hard maximum is the closest thing Windows has to memory.high. The
        // minimum stays soft so the process can still fault in what it needs.
        set_ws(&p, 1, cap as usize, QUOTA_LIMITS_HARDWS_MAX_ENABLE)?;
        Ok(SquashOutcome { before, after, cap })
    }

    fn uncap(&self, t: &Target) -> Result<()> {
        let Ok(pid) = pid_of(t) else { return Ok(()) };
        // The process exiting is the normal case, not an error.
        let Ok(p) = Proc::open(pid) else {
            return Ok(());
        };
        let (min, max, _) = get_ws(&p)?;
        set_ws(
            &p,
            min,
            max,
            QUOTA_LIMITS_HARDWS_MIN_DISABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE,
        )
    }

    fn pin(&self, t: &Target) -> Result<()> {
        let pid = pid_of(t)?;
        let p = Proc::open(pid)?;
        let rss = counters(&p)?.WorkingSetSize as usize;
        let (_, max, _) = get_ws(&p).unwrap_or((0, 0, 0));
        // Hard minimum at the current size keeps these pages resident; the
        // maximum is left soft so the app can still grow.
        let keep_max = max.max(rss.saturating_mul(2)).max(rss + (64 << 20));
        set_ws(
            &p,
            rss,
            keep_max,
            QUOTA_LIMITS_HARDWS_MIN_ENABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE,
        )
    }

    fn unpin(&self, t: &Target) -> Result<()> {
        let Ok(pid) = pid_of(t) else { return Ok(()) };
        let Ok(p) = Proc::open(pid) else {
            return Ok(());
        };
        let (min, max, _) = get_ws(&p)?;
        set_ws(
            &p,
            min,
            max,
            QUOTA_LIMITS_HARDWS_MIN_DISABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_key() {
        let t = Target {
            key: "not-a-pid".into(),
            label: String::new(),
            pids: vec![],
        };
        assert!(pid_of(&t).is_err());
    }

    #[test]
    fn never_targets_self() {
        let b = WindowsBackend;
        assert!(b.targets(&[std::process::id()]).is_empty());
    }

    #[test]
    fn reads_own_stats() {
        let b = WindowsBackend;
        let t = Target {
            key: std::process::id().to_string(),
            label: String::new(),
            pids: vec![],
        };
        let s = b.stats(&t).expect("should read our own counters");
        assert!(s.rss > 0);
    }

    #[test]
    fn refuses_to_squash_self() {
        let b = WindowsBackend;
        let t = Target {
            key: std::process::id().to_string(),
            label: String::new(),
            pids: vec![],
        };
        assert!(b.squash(&t, 0, 0).is_err());
    }
}
