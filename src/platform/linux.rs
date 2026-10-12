//! Linux backend: cgroup v2 proactive reclaim.
//!
//! Needs no privileges wherever systemd delegates the memory controller to the
//! user session, which is the default on modern distros - check with
//! `cat /sys/fs/cgroup/user.slice/user-$UID.slice/user@$UID.service/cgroup.subtree_control`.

use super::{AppStats, Backend, SquashOutcome, Target};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Stop chunked reclaim once a pass frees less than this - we're at the floor.
const RECLAIM_PROGRESS_FLOOR: u64 = 4 * 1024 * 1024;
const RECLAIM_MIN_CHUNK: u64 = 8 * 1024 * 1024;
const RECLAIM_MAX_PASSES: usize = 12;

pub struct LinuxBackend {
    /// Our own cgroup. Never squash or cap the group we live in: capping it
    /// would throttle the engine itself into uselessness.
    own_cgroup: Option<PathBuf>,
}

impl LinuxBackend {
    pub fn new() -> Result<Self> {
        let root = Path::new(CGROUP_ROOT);
        if !root.join("cgroup.controllers").exists() {
            bail!(
                "cgroup v2 is not mounted at {CGROUP_ROOT} (unified hierarchy required; \
                 a cgroup v1 or hybrid system cannot be supported)"
            );
        }
        Ok(Self {
            own_cgroup: app_cgroup_of(std::process::id()),
        })
    }

    fn is_own(&self, p: &Path) -> bool {
        self.own_cgroup.as_deref() == Some(p)
    }
}

/// The cgroup holding `pid`, walked up to the nearest systemd unit boundary.
///
/// The pid's own leaf may be a nested group an app made for itself; the
/// `.scope`/`.service` above it is the whole application, which is what the
/// user means when they name an app.
fn app_cgroup_of(pid: u32) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    // Unified hierarchy is the line with an empty controller list: "0::/path".
    let rel = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))?
        .trim()
        .trim_start_matches('/');
    let mut path = Path::new(CGROUP_ROOT).join(rel);
    loop {
        let name = path.file_name()?.to_string_lossy().to_string();
        if name.ends_with(".scope") || name.ends_with(".service") {
            return Some(path);
        }
        if !path.pop() || path == Path::new(CGROUP_ROOT) {
            return None;
        }
    }
}

fn read_u64(p: impl AsRef<Path>) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn read_trim(p: impl AsRef<Path>) -> Option<String> {
    Some(std::fs::read_to_string(p).ok()?.trim().to_string())
}

/// `memory.events` is a key/value table; pull one counter out of it.
fn event_count(dir: &Path, key: &str) -> u64 {
    std::fs::read_to_string(dir.join("memory.events"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix(key)?.trim().parse::<u64>().ok())
        })
        .unwrap_or(0)
}

fn cpu_usec(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join("cpu.stat"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("usage_usec")?.trim().parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// PSI: percentage of the last 10s in which *every* task in the group was
/// stalled waiting on memory.
fn stall_pct(dir: &Path) -> f32 {
    std::fs::read_to_string(dir.join("memory.pressure"))
        .ok()
        .and_then(|t| {
            let line = t.lines().find(|l| l.starts_with("full "))?;
            line.split_whitespace()
                .find_map(|f| f.strip_prefix("avg10=")?.parse::<f32>().ok())
        })
        .unwrap_or(0.0)
}

fn has_procs(dir: &Path) -> bool {
    // cgroup files report size 0 even when populated, so `len() > 0` is useless
    // here - the contents have to be read.
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false)
}

impl Backend for LinuxBackend {
    fn id(&self) -> &'static str {
        "linux-cgroup2"
    }

    fn mechanism(&self) -> String {
        "cgroup v2: memory.reclaim to evict, memory.high as the ceiling, \
         memory.swap.max=0 to pin"
            .into()
    }

    fn supports_stall(&self) -> bool {
        true
    }

    fn health(&self) -> Result<()> {
        let own = self
            .own_cgroup
            .as_deref()
            .context("could not determine our own cgroup")?;
        let parent = own.parent().context("our cgroup has no parent")?;
        let ctrl = read_trim(parent.join("cgroup.subtree_control")).unwrap_or_default();
        if !ctrl.split_whitespace().any(|c| c == "memory") {
            bail!(
                "the memory controller is not delegated to this cgroup ({}). \
                 Reclaim and ceilings will fail. On systemd, enable delegation \
                 for the user session.",
                parent.display()
            );
        }
        Ok(())
    }

    fn targets(&self, pids: &[u32]) -> Vec<Target> {
        // Many pids collapse into few cgroups; dedupe so one app is squashed once.
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        for &pid in pids {
            if let Some(cg) = app_cgroup_of(pid)
                && !self.is_own(&cg)
                && has_procs(&cg)
            {
                seen.insert(cg);
            }
        }
        seen.into_iter()
            .map(|cg| {
                let label = cg
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let pids = std::fs::read_to_string(cg.join("cgroup.procs"))
                    .map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).collect())
                    .unwrap_or_default();
                Target {
                    key: cg.to_string_lossy().to_string(),
                    label,
                    pids,
                }
            })
            .collect()
    }

    fn stats(&self, t: &Target) -> Option<AppStats> {
        let dir = Path::new(&t.key);
        if !dir.is_dir() {
            return None;
        }
        Some(AppStats {
            rss: read_u64(dir.join("memory.current"))?,
            swap: read_u64(dir.join("memory.swap.current")).unwrap_or(0),
            throttle_events: event_count(dir, "high"),
            cpu_usec: cpu_usec(dir),
            stall_pct: stall_pct(dir),
        })
    }

    fn squash(&self, t: &Target, headroom: u64, floor: u64) -> Result<SquashOutcome> {
        let dir = Path::new(&t.key);
        if self.is_own(dir) {
            bail!("refusing to squash our own cgroup");
        }
        let before = read_u64(dir.join("memory.current")).context("reading memory.current")?;

        // Chunked rather than one big write: a single oversized write returns
        // EAGAIN on partial success and tells us nothing about progress.
        let mut prev = before;
        for _ in 0..RECLAIM_MAX_PASSES {
            let chunk = (prev / 4).max(RECLAIM_MIN_CHUNK);
            let _ = std::fs::write(dir.join("memory.reclaim"), chunk.to_string());
            let now = read_u64(dir.join("memory.current")).unwrap_or(prev);
            if now + RECLAIM_PROGRESS_FLOOR >= prev {
                break;
            }
            prev = now;
        }

        let after = read_u64(dir.join("memory.current")).unwrap_or(before);
        let cap = (after + headroom).max(floor);
        std::fs::write(dir.join("memory.high"), cap.to_string())
            .with_context(|| format!("setting memory.high on {}", dir.display()))?;
        Ok(SquashOutcome { before, after, cap })
    }

    fn uncap(&self, t: &Target) -> Result<()> {
        let dir = Path::new(&t.key);
        if !dir.is_dir() {
            return Ok(()); // app exited; nothing to restore
        }
        std::fs::write(dir.join("memory.high"), "max")
            .with_context(|| format!("clearing memory.high on {}", dir.display()))?;
        Ok(())
    }

    fn pin(&self, t: &Target) -> Result<()> {
        let dir = Path::new(&t.key);
        std::fs::write(dir.join("memory.swap.max"), "0")
            .with_context(|| format!("pinning {}", dir.display()))?;
        Ok(())
    }

    fn unpin(&self, t: &Target) -> Result<()> {
        let dir = Path::new(&t.key);
        if !dir.is_dir() {
            return Ok(());
        }
        std::fs::write(dir.join("memory.swap.max"), "max")
            .with_context(|| format!("unpinning {}", dir.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_our_own_cgroup_and_it_is_a_unit() {
        let cg = app_cgroup_of(std::process::id());
        if let Some(p) = cg {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                name.ends_with(".scope") || name.ends_with(".service"),
                "walked-up cgroup should be a systemd unit, got {name}"
            );
            assert!(p.starts_with(CGROUP_ROOT));
        }
    }

    #[test]
    fn backend_refuses_to_squash_itself() {
        let Ok(b) = LinuxBackend::new() else { return };
        let Some(own) = b.own_cgroup.clone() else {
            return;
        };
        let t = Target {
            key: own.to_string_lossy().to_string(),
            label: "self".into(),
            pids: vec![],
        };
        assert!(b.squash(&t, 0, 0).is_err());
    }

    #[test]
    fn own_cgroup_is_never_a_target() {
        let Ok(b) = LinuxBackend::new() else { return };
        let targets = b.targets(&[std::process::id()]);
        assert!(targets.is_empty(), "we must never target ourselves");
    }

    #[test]
    fn stats_read_from_a_real_cgroup() {
        let Ok(b) = LinuxBackend::new() else { return };
        // pid 1 is in a cgroup on any systemd system.
        let targets = b.targets(&[1]);
        if let Some(t) = targets.first() {
            let s = b.stats(t).expect("stats for a live cgroup");
            assert!(s.rss > 0, "a live unit should have resident memory");
        }
    }
}
