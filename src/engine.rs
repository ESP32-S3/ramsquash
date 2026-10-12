//! The state machine.
//!
//! While a game is running, matched background apps are evicted to swap and
//! held under a ceiling. The instant one of them is *used* again it is released
//! and pages back in on demand; once it has been idle a while it is squashed
//! again. Apps on the exempt/never-swap lists are left alone throughout.

use crate::config::{Config, matches_any};
use crate::platform::{AppStats, Backend, Hold, HoldStore, Target, backend};
use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, System};

const LOG_LINES: usize = 300;
const MB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Game,
    Victim,
    Pinned,
    Exempt,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Game => "game",
            Role::Victim => "victim",
            Role::Pinned => "never-swap",
            Role::Exempt => "exempt",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub label: String,
    pub role: Role,
    pub rss: u64,
    pub swap: u64,
    pub state: String,
}

/// One process name, aggregated across all its processes - what the GUI's
/// picker lists, since users think in apps rather than pids.
#[derive(Clone, Debug)]
pub struct ProcSummary {
    pub name: String,
    pub rss: u64,
    pub count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub running: bool,
    pub backend: String,
    pub mechanism: String,
    pub health: Option<String>,
    pub game_active: bool,
    pub active_games: Vec<String>,
    pub rows: Vec<Row>,
    pub processes: Vec<ProcSummary>,
    pub total_ram: u64,
    pub used_ram: u64,
    pub total_swap: u64,
    pub used_swap: u64,
    pub freed_total: u64,
    pub log: Vec<String>,
}

/// Shared between the engine thread and the UI.
pub struct Shared {
    pub snapshot: Mutex<Snapshot>,
    pub config: Mutex<Config>,
    pub stop: AtomicBool,
    /// Set by the UI after editing `config` so the engine re-reads it.
    pub config_dirty: AtomicBool,
}

impl Shared {
    pub fn new(config: Config) -> Self {
        Self {
            snapshot: Mutex::new(Snapshot::default()),
            config: Mutex::new(config),
            stop: AtomicBool::new(false),
            config_dirty: AtomicBool::new(false),
        }
    }
}

/// Per-target bookkeeping. Deltas need a previous sample, and the timers are
/// what keep the engine from thrashing an app in and out of swap.
struct Tracked {
    label: String,
    prev: AppStats,
    sampled_at: Instant,
    last_active: Instant,
    squashed_at: Option<Instant>,
    held: bool,
    pinned: bool,
}

pub struct Engine {
    cfg: Config,
    backend: Box<dyn Backend>,
    sys: System,
    holds: HoldStore,
    tracked: HashMap<String, Tracked>,
    game_active: bool,
    last_sweep: Instant,
    log: VecDeque<String>,
    freed_total: u64,
    health: Option<String>,
    last_snapshot: Option<Snapshot>,
    /// Observe-only: never pin, squash, uncap, or touch the hold store.
    read_only: bool,
}

impl Engine {
    pub fn new(cfg: Config) -> Result<Self> {
        Self::build(cfg, false)
    }

    /// An engine that only looks. `status` and `list` must not change
    /// anything, and in particular must not restore holds out from under a
    /// daemon that is legitimately holding them.
    pub fn observer(cfg: Config) -> Result<Self> {
        Self::build(cfg, true)
    }

    fn build(cfg: Config, read_only: bool) -> Result<Self> {
        let backend = backend()?;
        let health = backend.health().err().map(|e| e.to_string());
        let mut e = Self {
            cfg,
            backend,
            sys: System::new(),
            holds: HoldStore::load(),
            tracked: HashMap::new(),
            game_active: false,
            last_sweep: Instant::now(),
            log: VecDeque::new(),
            freed_total: 0,
            health,
            last_snapshot: None,
            read_only,
        };
        // A process killed with SIGKILL never ran its cleanup, which would
        // leave the user's apps capped with nothing around to undo it.
        if !read_only && !e.holds.is_empty() {
            let n = e.holds.holds.len();
            e.restore_persisted();
            e.say(format!(
                "cleared {n} stale hold(s) from a previous unclean exit"
            ));
        }
        if let Some(h) = e.health.clone() {
            e.say(format!("WARNING: {h}"));
        }
        Ok(e)
    }

    fn say(&mut self, msg: impl Into<String>) {
        let line = msg.into();
        eprintln!("{line}");
        self.log.push_back(line);
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    /// Undo everything named in the durable hold store, whether or not this
    /// process is the one that set it.
    fn restore_persisted(&mut self) {
        let holds = std::mem::take(&mut self.holds.holds);
        for (key, hold) in holds {
            let t = Target {
                key: key.clone(),
                label: key,
                pids: vec![],
            };
            if hold.original_cap.is_some() {
                let _ = self.backend.uncap(&t);
            }
            if hold.pinned {
                let _ = self.backend.unpin(&t);
            }
        }
        let _ = self.holds.save();
    }

    fn note_hold(&mut self, key: &str, f: impl FnOnce(&mut Hold)) {
        if self.read_only {
            return;
        }
        let h = self.holds.holds.entry(key.to_string()).or_default();
        f(h);
        if h == &Hold::default() {
            self.holds.holds.remove(key);
        }
        let _ = self.holds.save();
    }

    /// Build an engine around a caller-supplied backend. Used by the tests to
    /// drive the state machine deterministically.
    #[cfg(test)]
    fn with_backend(cfg: Config, backend: Box<dyn Backend>) -> Self {
        Self {
            cfg,
            backend,
            sys: System::new(),
            holds: HoldStore::default(),
            tracked: HashMap::new(),
            game_active: false,
            last_sweep: Instant::now(),
            log: VecDeque::new(),
            freed_total: 0,
            health: None,
            last_snapshot: None,
            read_only: false,
        }
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.cfg = cfg;
    }

    // -- classification ----------------------------------------------------

    /// Names a rule can be written against: the process name and its exe's
    /// file name, so both "firefox" and "firefox.exe" work on either platform.
    /// The name to show and aggregate under. `comm` is truncated to 15 chars
    /// on Linux and is often a thread label ("DOM Worker"), so the
    /// executable's stem is a far better identity for a human picking apps.
    fn display_name(p: &sysinfo::Process) -> String {
        p.exe()
            .and_then(|e| e.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| p.name().to_string_lossy().to_string())
    }

    fn candidates(p: &sysinfo::Process) -> Vec<String> {
        let mut v = vec![p.name().to_string_lossy().to_string()];
        let mut add = |s: String| {
            if !s.is_empty() && !v.contains(&s) {
                v.push(s);
            }
        };
        if let Some(exe) = p.exe().and_then(|e| e.file_name()) {
            let file = exe.to_string_lossy().to_string();
            // Also offer the name without its extension, so one rule written
            // as "firefox" matches both firefox and firefox.exe.
            if let Some(stem) = std::path::Path::new(&file).file_stem() {
                add(stem.to_string_lossy().to_string());
            }
            add(file);
        }
        v
    }

    pub fn tick(&mut self) {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_memory()
                .with_exe(sysinfo::UpdateKind::OnlyIfNotSet),
        );
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram().with_swap());

        let mut game_pids = Vec::new();
        let mut victim_pids = Vec::new();
        let mut pinned_pids = Vec::new();
        let mut exempt_pids = Vec::new();
        let mut game_names: Vec<String> = Vec::new();
        let mut summaries: HashMap<String, (u64, usize)> = HashMap::new();

        for (pid, proc_) in self.sys.processes() {
            // Threads share their process's RSS, so counting them would both
            // list meaningless names and multiply the memory figures.
            if proc_.thread_kind().is_some() {
                continue;
            }
            let cands = Self::candidates(proc_);
            let name = Self::display_name(proc_);
            let e = summaries.entry(name.clone()).or_insert((0, 0));
            e.0 += proc_.memory();
            e.1 += 1;

            let pid_u = pid.as_u32();
            if matches_any(&self.cfg.never_swap, &cands) {
                pinned_pids.push(pid_u);
                continue; // never a victim
            }
            if self.cfg.is_exempt(&cands) {
                exempt_pids.push(pid_u);
                continue;
            }
            if matches_any(&self.cfg.games, &cands) {
                // A launcher or a leftover helper holding a few MB is not a
                // running game; require real memory before acting on anything.
                if proc_.memory() >= self.cfg.min_game_mb * MB {
                    game_pids.push(pid_u);
                    if !game_names.contains(&name) {
                        game_names.push(name);
                    }
                }
                continue; // games are never victims
            }
            if matches_any(&self.cfg.victims, &cands) {
                victim_pids.push(pid_u);
            }
        }

        let game_now = !game_pids.is_empty();
        let victims = self.backend.targets(&victim_pids);

        if self.read_only {
            self.game_active = game_now;
            let games = self.backend.targets(&game_pids);
            let exempt = self.backend.targets(&exempt_pids);
            let pinned = self.backend.targets(&pinned_pids);
            self.publish(game_names, games, exempt, victims, pinned, summaries);
            return;
        }

        // Pins are unconditional: not gated on a game, not gated on a call.
        // Latency-critical apps accumulate pagefile residency during ordinary
        // desktop use, entirely without a game involved.
        let pinned = self.backend.targets(&pinned_pids);
        for t in &pinned {
            if !self.tracked.get(&t.key).map(|x| x.pinned).unwrap_or(false) {
                match self.backend.pin(t) {
                    Ok(()) => {
                        self.say(format!("pinned {} fully in RAM (never swapped)", t.label));
                        let key = t.key.clone();
                        self.note_hold(&key, |h| {
                            h.pinned = true;
                            h.permanent_pin = true;
                        });
                        self.track(t).pinned = true;
                    }
                    Err(e) => self.say(format!("could not pin {}: {e}", t.label)),
                }
            }
        }

        if game_now && !self.game_active {
            self.say(format!(
                "game detected ({}) -> evicting background apps",
                game_names.join(", ")
            ));
            if self.cfg.pin_game_in_ram {
                for t in self.backend.targets(&game_pids) {
                    if self.backend.pin(&t).is_ok() {
                        let key = t.key.clone();
                        self.note_hold(&key, |h| h.pinned = true);
                        self.say(format!("pinned game {} in RAM", t.label));
                    }
                }
            }
            for t in &victims {
                self.squash(t, true);
            }
            self.last_sweep = Instant::now();
        } else if !game_now && self.game_active {
            self.say("game gone -> lifting ceilings");
            self.release_caps_and_game_pins();
        }
        self.game_active = game_now;

        if game_now {
            for t in &victims {
                self.evaluate(t);
            }
            if self.last_sweep.elapsed() >= Duration::from_secs(self.cfg.resquash_secs) {
                for t in &victims {
                    if self.tracked.get(&t.key).map(|x| x.held).unwrap_or(false) {
                        self.squash(t, false);
                    }
                }
                self.last_sweep = Instant::now();
            }
        }

        let games = self.backend.targets(&game_pids);
        let exempt = self.backend.targets(&exempt_pids);
        self.prune(&victims, &pinned);
        self.publish(game_names, games, exempt, victims, pinned, summaries);
    }

    fn track(&mut self, t: &Target) -> &mut Tracked {
        let now = Instant::now();
        self.tracked
            .entry(t.key.clone())
            .or_insert_with(|| Tracked {
                label: t.label.clone(),
                prev: AppStats::default(),
                sampled_at: now,
                last_active: now,
                squashed_at: None,
                held: false,
                pinned: false,
            })
    }

    fn squash(&mut self, t: &Target, verbose: bool) {
        let headroom = self.cfg.headroom_mb * MB;
        let floor = self.cfg.floor_mb * MB;
        match self.backend.squash(t, headroom, floor) {
            Ok(o) => {
                self.freed_total += o.freed();
                if verbose || o.freed() > 8 * MB {
                    self.say(format!(
                        "squashed {}: {}M -> {}M (ceiling {}M)",
                        t.label,
                        o.before / MB,
                        o.after / MB,
                        o.cap / MB
                    ));
                }
                let key = t.key.clone();
                self.note_hold(&key, |h| h.original_cap = Some("max".into()));
                let stats = self.backend.stats(t).unwrap_or_default();
                let e = self.track(t);
                e.held = true;
                e.squashed_at = Some(Instant::now());
                // Re-baseline: reclaim charges its own swap-out work to the
                // app, so a sample taken before it settles reads as activity.
                e.prev = stats;
                e.sampled_at = Instant::now();
            }
            Err(e) => self.say(format!("could not squash {}: {e}", t.label)),
        }
    }

    /// Decide whether this target is being used, and wake or re-squash it.
    fn evaluate(&mut self, t: &Target) {
        let Some(stats) = self.backend.stats(t) else {
            return;
        };
        let settle = Duration::from_secs(self.cfg.settle_secs);
        let grace = Duration::from_secs(self.cfg.idle_grace_secs);
        let (wake_ev, wake_cpu, stall_lim) = (
            self.cfg.wake_throttle_events,
            self.cfg.wake_cpu_pct,
            self.cfg.stall_pct,
        );
        let supports_stall = self.backend.supports_stall();

        let existed = self.tracked.contains_key(&t.key);
        let e = self.track(t);
        if !existed {
            // First sighting mid-game: you probably just launched it, so give
            // it the idle grace period instead of squashing it immediately.
            e.prev = stats;
            e.sampled_at = Instant::now();
            e.last_active = Instant::now();
            let label = t.label.clone();
            self.say(format!("new app {label} - grace period, not squashing yet"));
            return;
        }

        if let Some(at) = e.squashed_at
            && at.elapsed() < settle
        {
            e.prev = stats;
            e.sampled_at = Instant::now();
            return;
        }

        let elapsed = e.sampled_at.elapsed().as_secs_f32().max(0.001);
        let d_ev = stats.throttle_events.saturating_sub(e.prev.throttle_events);
        let d_cpu = stats.cpu_usec.saturating_sub(e.prev.cpu_usec);
        let cpu_pct = (d_cpu as f32 / 1_000_000.0) / elapsed * 100.0;
        let stalled = supports_stall && stats.stall_pct >= stall_lim;

        e.prev = stats;
        e.sampled_at = Instant::now();

        let active = d_ev >= wake_ev || cpu_pct >= wake_cpu || stalled;
        if active {
            e.last_active = Instant::now();
        }
        let held = e.held;
        let idle_for = e.last_active.elapsed();
        let label = e.label.clone();

        if held && active {
            e.held = false;
            e.squashed_at = None;
            let _ = self.backend.uncap(t);
            let key = t.key.clone();
            self.note_hold(&key, |h| h.original_cap = None);
            let why = if stalled {
                "stalled at ceiling"
            } else {
                "in use"
            };
            self.say(format!("woke {label} ({why}) - ceiling lifted"));
        } else if !held && !active && idle_for >= grace {
            self.squash(t, true);
        }
    }

    fn release_caps_and_game_pins(&mut self) {
        let holds: Vec<(String, Hold)> = self
            .holds
            .holds
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, hold) in holds {
            let t = Target {
                key: key.clone(),
                label: key.clone(),
                pids: vec![],
            };
            if hold.original_cap.is_some() {
                let _ = self.backend.uncap(&t);
                self.note_hold(&key, |h| h.original_cap = None);
            }
            // A never-swap pin outlives the game; only the game's own does not.
            if hold.pinned && !hold.permanent_pin {
                let _ = self.backend.unpin(&t);
                self.note_hold(&key, |h| h.pinned = false);
            }
        }
        for e in self.tracked.values_mut() {
            e.held = false;
            e.squashed_at = None;
        }
    }

    /// Forget targets that have gone away, so a long session doesn't grow
    /// bookkeeping for every app that ever ran.
    fn prune(&mut self, victims: &[Target], pinned: &[Target]) {
        let live: std::collections::HashSet<&String> =
            victims.iter().chain(pinned).map(|t| &t.key).collect();
        let dead: Vec<String> = self
            .tracked
            .keys()
            .filter(|k| !live.contains(k))
            .cloned()
            .collect();
        for k in dead {
            self.tracked.remove(&k);
            if self.holds.holds.contains_key(&k) {
                self.holds.holds.remove(&k);
                let _ = self.holds.save();
            }
        }
    }

    fn publish(
        &mut self,
        active_games: Vec<String>,
        games: Vec<Target>,
        exempt: Vec<Target>,
        victims: Vec<Target>,
        pinned: Vec<Target>,
        summaries: HashMap<String, (u64, usize)>,
    ) {
        let mut rows = Vec::new();
        for t in &games {
            let s = self.backend.stats(t).unwrap_or_default();
            rows.push(Row {
                label: t.label.clone(),
                role: Role::Game,
                rss: s.rss,
                swap: s.swap,
                state: if self.cfg.pin_game_in_ram {
                    "running, kept in RAM".into()
                } else {
                    "running".into()
                },
            });
        }
        for t in &exempt {
            let s = self.backend.stats(t).unwrap_or_default();
            rows.push(Row {
                label: t.label.clone(),
                role: Role::Exempt,
                rss: s.rss,
                swap: s.swap,
                state: "left alone".into(),
            });
        }
        for t in &pinned {
            let s = self.backend.stats(t).unwrap_or_default();
            rows.push(Row {
                label: t.label.clone(),
                role: Role::Pinned,
                rss: s.rss,
                swap: s.swap,
                state: "pinned in RAM".into(),
            });
        }
        for t in &victims {
            let s = self.backend.stats(t).unwrap_or_default();
            let held = self.tracked.get(&t.key).map(|x| x.held).unwrap_or(false);
            rows.push(Row {
                label: t.label.clone(),
                role: Role::Victim,
                rss: s.rss,
                swap: s.swap,
                state: if held { "held".into() } else { "free".into() },
            });
        }
        let mut processes: Vec<ProcSummary> = summaries
            .into_iter()
            .map(|(name, (rss, count))| ProcSummary { name, rss, count })
            .collect();
        processes.sort_by_key(|a| std::cmp::Reverse(a.rss));
        processes.truncate(400);

        let snap = Snapshot {
            running: true,
            backend: self.backend.id().to_string(),
            mechanism: self.backend.mechanism(),
            health: self.health.clone(),
            game_active: self.game_active,
            active_games,
            rows,
            processes,
            total_ram: self.sys.total_memory(),
            used_ram: self.sys.used_memory(),
            total_swap: self.sys.total_swap(),
            used_swap: self.sys.used_swap(),
            freed_total: self.freed_total,
            log: self.log.iter().cloned().collect(),
        };
        self.last_snapshot = Some(snap);
    }

    pub fn take_snapshot(&mut self) -> Option<Snapshot> {
        self.last_snapshot.take()
    }

    /// Release every ceiling and pin. Must run on any orderly exit.
    pub fn shutdown(&mut self) {
        if self.read_only {
            return;
        }
        self.say("shutting down - releasing all holds");
        self.restore_persisted();
    }
}

/// Run the engine until `shared.stop` is set. Always releases holds on exit.
pub fn run(shared: Arc<Shared>) {
    let cfg = shared.config.lock().unwrap().clone();
    let mut engine = match Engine::new(cfg) {
        Ok(e) => e,
        Err(e) => {
            let mut s = shared.snapshot.lock().unwrap();
            s.running = false;
            s.health = Some(e.to_string());
            return;
        }
    };
    while !shared.stop.load(Ordering::Relaxed) {
        if shared.config_dirty.swap(false, Ordering::Relaxed) {
            engine.set_config(shared.config.lock().unwrap().clone());
        }
        engine.tick();
        if let Some(snap) = engine.take_snapshot() {
            *shared.snapshot.lock().unwrap() = snap;
        }
        let tick = engine.cfg.tick_secs.max(1);
        // Wake promptly on stop rather than sleeping out the whole interval.
        for _ in 0..(tick * 10) {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    engine.shutdown();
    let mut s = shared.snapshot.lock().unwrap();
    s.running = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::SquashOutcome;
    use std::sync::atomic::AtomicU64;

    /// A backend whose numbers the test drives directly.
    #[derive(Default)]
    struct Mock {
        rss: AtomicU64,
        throttles: AtomicU64,
        cpu: AtomicU64,
        stall: AtomicU64,  // whole percent
        capped: AtomicU64, // 1 = a ceiling is set
        pinned: AtomicU64,
    }

    impl Backend for Mock {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn mechanism(&self) -> String {
            "mock".into()
        }
        fn supports_stall(&self) -> bool {
            true
        }
        fn targets(&self, pids: &[u32]) -> Vec<Target> {
            pids.iter()
                .map(|p| Target {
                    key: p.to_string(),
                    label: format!("app{p}"),
                    pids: vec![*p],
                })
                .collect()
        }
        fn stats(&self, _t: &Target) -> Option<AppStats> {
            Some(AppStats {
                rss: self.rss.load(Ordering::Relaxed),
                swap: 0,
                throttle_events: self.throttles.load(Ordering::Relaxed),
                cpu_usec: self.cpu.load(Ordering::Relaxed),
                stall_pct: self.stall.load(Ordering::Relaxed) as f32,
            })
        }
        fn squash(&self, _t: &Target, headroom: u64, floor: u64) -> Result<SquashOutcome> {
            let before = self.rss.load(Ordering::Relaxed);
            self.rss.store(MB, Ordering::Relaxed);
            self.capped.store(1, Ordering::Relaxed);
            Ok(SquashOutcome {
                before,
                after: MB,
                cap: (MB + headroom).max(floor),
            })
        }
        fn uncap(&self, _t: &Target) -> Result<()> {
            self.capped.store(0, Ordering::Relaxed);
            Ok(())
        }
        fn pin(&self, _t: &Target) -> Result<()> {
            self.pinned.store(1, Ordering::Relaxed);
            Ok(())
        }
        fn unpin(&self, _t: &Target) -> Result<()> {
            self.pinned.store(0, Ordering::Relaxed);
            Ok(())
        }
    }

    fn target() -> Target {
        Target {
            key: "1".into(),
            label: "app1".into(),
            pids: vec![1],
        }
    }

    fn engine_with(cfg: Config) -> (Engine, Arc<Mock>) {
        let mock = Arc::new(Mock::default());
        mock.rss.store(400 * MB, Ordering::Relaxed);
        struct Shim(Arc<Mock>);
        impl Backend for Shim {
            fn id(&self) -> &'static str {
                self.0.id()
            }
            fn mechanism(&self) -> String {
                self.0.mechanism()
            }
            fn supports_stall(&self) -> bool {
                self.0.supports_stall()
            }
            fn targets(&self, p: &[u32]) -> Vec<Target> {
                self.0.targets(p)
            }
            fn stats(&self, t: &Target) -> Option<AppStats> {
                self.0.stats(t)
            }
            fn squash(&self, t: &Target, h: u64, f: u64) -> Result<SquashOutcome> {
                self.0.squash(t, h, f)
            }
            fn uncap(&self, t: &Target) -> Result<()> {
                self.0.uncap(t)
            }
            fn pin(&self, t: &Target) -> Result<()> {
                self.0.pin(t)
            }
            fn unpin(&self, t: &Target) -> Result<()> {
                self.0.unpin(t)
            }
        }
        let e = Engine::with_backend(cfg, Box::new(Shim(mock.clone())));
        (e, mock)
    }

    fn fast_cfg() -> Config {
        Config {
            settle_secs: 0,
            idle_grace_secs: 1,
            wake_throttle_events: 20,
            wake_cpu_pct: 15.0,
            stall_pct: 10.0,
            headroom_mb: 64,
            floor_mb: 32,
            ..Default::default()
        }
    }

    #[test]
    fn squash_then_wake_on_throttle_events() {
        let (mut e, m) = engine_with(fast_cfg());
        let t = target();
        e.squash(&t, true);
        assert_eq!(
            m.capped.load(Ordering::Relaxed),
            1,
            "should be capped after squash"
        );
        assert!(e.tracked[&t.key].held);

        // Simulate the app faulting its working set back in.
        m.throttles.fetch_add(500, Ordering::Relaxed);
        e.evaluate(&t);
        assert_eq!(
            m.capped.load(Ordering::Relaxed),
            0,
            "ceiling must lift when the app is used"
        );
        assert!(!e.tracked[&t.key].held);
    }

    #[test]
    fn wakes_a_stalled_app_even_without_throttle_events() {
        let (mut e, m) = engine_with(fast_cfg());
        let t = target();
        e.squash(&t, true);
        // No throttle events and no CPU: only pressure says it is thrashing.
        m.stall.store(80, Ordering::Relaxed);
        e.evaluate(&t);
        assert_eq!(
            m.capped.load(Ordering::Relaxed),
            0,
            "a thrashing app must escape its ceiling via the pressure signal"
        );
    }

    #[test]
    fn settle_window_suppresses_the_squashs_own_cost() {
        let mut cfg = fast_cfg();
        cfg.settle_secs = 60; // squash just happened; nothing should wake it
        let (mut e, m) = engine_with(cfg);
        let t = target();
        e.squash(&t, true);
        m.cpu.fetch_add(10_000_000, Ordering::Relaxed); // reclaim's own CPU
        m.throttles.fetch_add(10_000, Ordering::Relaxed);
        e.evaluate(&t);
        assert_eq!(
            m.capped.load(Ordering::Relaxed),
            1,
            "activity during the settle window must not wake the app"
        );
    }

    #[test]
    fn idle_app_is_resquashed_after_the_grace_period() {
        let (mut e, m) = engine_with(fast_cfg());
        let t = target();
        e.squash(&t, true);
        m.throttles.fetch_add(500, Ordering::Relaxed);
        e.evaluate(&t);
        assert_eq!(m.capped.load(Ordering::Relaxed), 0);

        m.rss.store(400 * MB, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(1200)); // exceed idle_grace_secs = 1
        e.evaluate(&t); // no activity recorded this round
        assert_eq!(
            m.capped.load(Ordering::Relaxed),
            1,
            "an idle app should be parked again"
        );
    }

    #[test]
    fn first_sighting_gets_a_grace_period_not_an_immediate_squash() {
        let (mut e, m) = engine_with(fast_cfg());
        let t = target();
        e.evaluate(&t); // never seen before
        assert_eq!(
            m.capped.load(Ordering::Relaxed),
            0,
            "an app seen for the first time mid-game is probably one you just opened"
        );
    }

    #[test]
    fn read_only_engine_never_mutates() {
        let (mut e, m) = engine_with(fast_cfg());
        e.read_only = true;
        let t = target();
        // Even asked directly, the hold store stays untouched...
        e.note_hold("1", |h| h.pinned = true);
        assert!(
            e.holds.holds.is_empty(),
            "observer mode must not record holds"
        );
        // ...and shutdown must not undo a running daemon's work.
        m.capped.store(1, Ordering::Relaxed);
        m.pinned.store(1, Ordering::Relaxed);
        e.shutdown();
        assert_eq!(
            m.capped.load(Ordering::Relaxed),
            1,
            "observer must not uncap"
        );
        assert_eq!(
            m.pinned.load(Ordering::Relaxed),
            1,
            "observer must not unpin"
        );
        let _ = t;
    }

    #[test]
    fn shutdown_releases_every_hold() {
        let (mut e, m) = engine_with(fast_cfg());
        let t = target();
        e.squash(&t, true);
        let _ = e.backend.pin(&t);
        e.note_hold("1", |h| h.pinned = true);
        e.shutdown();
        assert_eq!(
            m.capped.load(Ordering::Relaxed),
            0,
            "ceilings must be lifted on exit"
        );
        assert_eq!(
            m.pinned.load(Ordering::Relaxed),
            0,
            "pins must be released on exit"
        );
    }
}
