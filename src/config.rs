//! Configuration: the four rule lists plus tuning knobs, stored as TOML.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Case-insensitive glob supporting `*` (any run) and `?` (one char).
/// Deliberately tiny - process names don't need a full glob engine.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    // dp[j] = "pattern[..j] can match the text consumed so far"
    let mut dp = vec![false; p.len() + 1];
    dp[0] = true;
    for j in 1..=p.len() {
        dp[j] = dp[j - 1] && p[j - 1] == '*';
    }
    for &tc in &t {
        let mut next = vec![false; p.len() + 1];
        for j in 1..=p.len() {
            next[j] = match p[j - 1] {
                '*' => dp[j] || next[j - 1],
                '?' => dp[j - 1],
                pc => dp[j - 1] && pc == tc,
            };
        }
        dp = next;
    }
    dp[p.len()]
}

/// True if any pattern matches any of the candidate strings for a process
/// (its name, and its executable's file name).
pub fn matches_any(patterns: &[String], candidates: &[String]) -> bool {
    patterns.iter().any(|p| {
        let p = p.trim();
        !p.is_empty() && candidates.iter().any(|c| glob_match(p, c))
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Processes whose presence means "a game is running".
    pub games: Vec<String>,
    /// Processes to push out to swap while a game runs.
    pub victims: Vec<String>,
    /// Never reclaimed, whatever else matches. Wins over `victims`.
    pub exempt: Vec<String>,
    /// Pinned fully in RAM for as long as the engine runs - not just during a
    /// game. Implies `exempt`.
    pub never_swap: Vec<String>,

    /// A game process must hold at least this much RAM to count as really
    /// running, so launchers and leftover helpers don't trigger everything.
    pub min_game_mb: u64,
    /// After squashing, an app is capped at (post-reclaim size + this).
    pub headroom_mb: u64,
    /// Never cap an app below this.
    pub floor_mb: u64,

    pub tick_secs: u64,
    /// How long a woken app must sit idle before it is squashed again.
    pub idle_grace_secs: u64,
    /// Ignore activity for this long after squashing: reclaim's own cost
    /// otherwise reads as activity and wakes the app straight back up.
    pub settle_secs: u64,
    /// Top-up reclaim on still-held, still-idle apps this often.
    pub resquash_secs: u64,

    /// Memory-throttle events in one tick that mean "this app is in use".
    /// On Linux these are cgroup `memory.events` high events; on Windows they
    /// are hard page faults, which are far more numerous - hence the separate
    /// default applied at load time.
    pub wake_throttle_events: u64,
    /// CPU use in one tick, as a percentage of one core, that means "in use".
    pub wake_cpu_pct: f32,
    /// Wake a held app that spends this % of the last 10s fully stalled on
    /// memory. Escape hatch: a thrashing app can be too stuck to generate
    /// throttle events. Linux only.
    pub stall_pct: f32,

    /// Forbid the game itself from being swapped while it runs.
    pub pin_game_in_ram: bool,
    /// Start the engine as soon as the GUI opens.
    pub autostart_engine: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            games: default_games(),
            victims: default_victims(),
            exempt: default_exempt(),
            never_swap: default_exempt(),
            min_game_mb: 300,
            headroom_mb: 192,
            floor_mb: 64,
            tick_secs: 2,
            idle_grace_secs: 90,
            settle_secs: 5,
            resquash_secs: 30,
            wake_throttle_events: if cfg!(windows) { 2000 } else { 20 },
            wake_cpu_pct: 15.0,
            stall_pct: 10.0,
            pin_game_in_ram: true,
            autostart_engine: false,
        }
    }
}

fn default_games() -> Vec<String> {
    [
        "sober",
        "RobloxPlayerBeta*", // Roblox
        "steam_app_*",
        "gamescope",
        "csgo*",
        "cs2",
        "dota2",
        "factorio",
        "Minecraft*",
        "javaw",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_victims() -> Vec<String> {
    [
        "firefox",
        "chrome",
        "chromium",
        "Code",
        "code",
        "slack",
        "Obsidian",
        "thunderbird",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Voice and other latency-critical apps. Being paged out mid-call means
/// dropouts, and reclaiming a few hundred MB is not worth that.
fn default_exempt() -> Vec<String> {
    [
        "Discord",
        "discord",
        "WebexHost",
        "Teams",
        "ms-teams",
        "zoom",
        "mumble",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Config {
    pub fn config_dir() -> Result<PathBuf> {
        let d = dirs::config_dir().context("no config directory for this platform")?;
        Ok(d.join("ramsquash"))
    }

    pub fn path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    /// Load from disk, falling back to defaults when absent. A corrupt file is
    /// an error rather than a silent reset - losing someone's rule lists
    /// because of a stray character is worse than refusing to start.
    pub fn load() -> Result<Self> {
        let p = Self::path()?;
        if !p.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let p = Self::path()?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        // Write-then-rename so an interrupted save can't truncate the config.
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("replacing {}", p.display()))?;
        Ok(p)
    }

    /// `never_swap` implies exemption from reclaim, so callers only need one check.
    pub fn is_exempt(&self, candidates: &[String]) -> bool {
        matches_any(&self.exempt, candidates) || matches_any(&self.never_swap, candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("firefox", "firefox"));
        assert!(glob_match("FireFox", "firefox"), "must be case-insensitive");
        assert!(glob_match("steam_app_*", "steam_app_730"));
        assert!(glob_match("*.exe", "RobloxPlayerBeta.exe"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("cs?", "cs2"));
        assert!(!glob_match("firefox", "firefoxx"));
        assert!(!glob_match("steam_app_*", "steam"));
        assert!(!glob_match("cs?", "cs20"));
    }

    #[test]
    fn glob_handles_multiple_stars() {
        assert!(glob_match("*vinegar*", "org.vinegarhq.Vinegar.scope"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn empty_pattern_never_matches_nonempty() {
        assert!(!matches_any(&["".to_string()], &["firefox".to_string()]));
        assert!(!matches_any(&["  ".to_string()], &["firefox".to_string()]));
    }

    #[test]
    fn never_swap_implies_exempt() {
        let mut c = Config::default();
        c.exempt.clear();
        c.never_swap = vec!["Discord".into()];
        assert!(c.is_exempt(&["discord".to_string()]));
    }

    #[test]
    fn roundtrips_through_toml() {
        let c = Config::default();
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }
}
