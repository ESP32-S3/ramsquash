//! Platform abstraction.
//!
//! Both backends implement the same two-step idea:
//!   1. evict an app's pages to the pagefile/swap *now*, and
//!   2. hold a ceiling on it so it cannot quietly page everything back in.
//!
//! On top of that sits a "pin" that forbids eviction entirely, for
//! latency-critical apps such as voice chat.
//!
//! Linux does this with cgroup v2 (`memory.reclaim`, `memory.high`,
//! `memory.swap.max`), unprivileged wherever systemd delegates the memory
//! controller. Windows does it with `EmptyWorkingSet` and a hard working-set
//! maximum via `SetProcessWorkingSetSizeEx`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
pub mod windows;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppStats {
    /// Resident bytes.
    pub rss: u64,
    /// Bytes currently evicted to swap/pagefile. An estimate on Windows.
    pub swap: u64,
    /// Monotonic counter that rises when the app pushes against its ceiling.
    /// Linux: cgroup `memory.events` high events. Windows: hard page faults.
    pub throttle_events: u64,
    /// Monotonic CPU time in microseconds.
    pub cpu_usec: u64,
    /// % of the last 10s spent fully stalled on memory. Linux only; 0 elsewhere.
    pub stall_pct: f32,
}

#[derive(Clone, Debug)]
pub struct SquashOutcome {
    pub before: u64,
    pub after: u64,
    pub cap: u64,
}

impl SquashOutcome {
    pub fn freed(&self) -> u64 {
        self.before.saturating_sub(self.after)
    }
}

/// A unit the backend can act on: one cgroup on Linux, one process on Windows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Target {
    /// Backend-specific identity, stable from tick to tick.
    pub key: String,
    /// Human-readable name for the UI.
    pub label: String,
    pub pids: Vec<u32>,
}

pub trait Backend: Send + Sync {
    fn id(&self) -> &'static str;
    /// One line describing the mechanism, shown in the UI.
    fn mechanism(&self) -> String;
    /// Whether `AppStats::stall_pct` carries real data.
    fn supports_stall(&self) -> bool {
        false
    }
    /// Anything the user should know before relying on this backend, e.g. a
    /// missing kernel feature or a privilege that is not held.
    fn health(&self) -> Result<()> {
        Ok(())
    }
    /// Collapse raw pids into the units this backend acts on.
    fn targets(&self, pids: &[u32]) -> Vec<Target>;
    fn stats(&self, t: &Target) -> Option<AppStats>;
    /// Evict now, then cap at (post-eviction size + headroom), floored.
    fn squash(&self, t: &Target, headroom: u64, floor: u64) -> Result<SquashOutcome>;
    /// Lift the ceiling, letting the app page back in on demand.
    fn uncap(&self, t: &Target) -> Result<()>;
    /// Forbid eviction entirely.
    fn pin(&self, t: &Target) -> Result<()>;
    fn unpin(&self, t: &Target) -> Result<()>;
}

pub fn backend() -> Result<Box<dyn Backend>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxBackend::new()?))
    }
    #[cfg(windows)]
    {
        Ok(Box::new(windows::WindowsBackend::new()?))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        anyhow::bail!("ramsquash supports Linux and Windows only")
    }
}

// ---------------------------------------------------------------------------
// Crash recovery
// ---------------------------------------------------------------------------

/// What we changed about one target, so it can be put back.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Hold {
    /// Backend-encoded original ceiling, if we changed it.
    pub original_cap: Option<String>,
    /// True if we are forbidding eviction.
    pub pinned: bool,
    /// Set for pins that must survive a game exiting (`never_swap`).
    pub permanent_pin: bool,
}

/// Durable record of every ceiling and pin we are holding.
///
/// Without this, a SIGKILL (or a power cut) leaves the user's apps capped with
/// nothing left running that knows to undo it. Restoring on next start is the
/// difference between a tunable and a footgun.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HoldStore {
    #[serde(default)]
    pub holds: BTreeMap<String, Hold>,
}

impl HoldStore {
    pub fn path() -> Result<PathBuf> {
        let dir = dirs::runtime_dir()
            .or_else(dirs::cache_dir)
            .unwrap_or_else(std::env::temp_dir);
        Ok(dir.join("ramsquash").join("holds.toml"))
    }

    pub fn load() -> Self {
        let Ok(p) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(p)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.holds.is_empty()
    }
}
