//! ramsquash - keep a game's RAM free by parking idle background apps in swap.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod engine;
#[cfg(feature = "gui")]
mod gui;
mod platform;

use anyhow::Result;
use config::Config;
use engine::{Engine, Shared};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const MB: u64 = 1024 * 1024;

const HELP: &str = "\
ramsquash - park idle background apps in swap while a game runs

USAGE:
    ramsquash [COMMAND]

COMMANDS:
    (none)      open the GUI (falls back to `status` in headless builds)
    daemon      run the engine in the foreground; for a service unit
    status      print current state once and exit
    list        list running processes by memory use, for writing rules
    release     lift every ceiling and pin, then exit
    config      print the config file path
    help        this text
    version     print the version
";

fn main() -> Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.trim_start_matches('-') {
        "" => default_action(),
        "daemon" => daemon(),
        "status" => status(),
        "list" => list(),
        "release" => release(),
        "config" => {
            let p = Config::path()?;
            if !p.exists() {
                // Give people something to edit rather than a path to a file
                // that is not there yet.
                Config::default().save()?;
                println!("wrote a default config to {}", p.display());
            } else {
                println!("{}", p.display());
            }
            Ok(())
        }
        "version" | "V" => {
            println!("ramsquash {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "h" => {
            print!("{HELP}");
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print!("{HELP}");
            std::process::exit(2);
        }
    }
}

#[cfg(feature = "gui")]
fn default_action() -> Result<()> {
    gui::run()
}

#[cfg(not(feature = "gui"))]
fn default_action() -> Result<()> {
    eprintln!("(headless build - no GUI; showing status)\n");
    status()
}

fn daemon() -> Result<()> {
    let shared = Arc::new(Shared::new(Config::load()?));
    let s2 = shared.clone();
    // Without this, a Ctrl-C leaves every ceiling in place.
    ctrlc::set_handler(move || s2.stop.store(true, Ordering::Relaxed))?;
    engine::run(shared);
    Ok(())
}

fn human(bytes: u64) -> String {
    if bytes >= 1024 * MB {
        format!("{:.1}G", bytes as f64 / (1024.0 * MB as f64))
    } else {
        format!("{}M", bytes / MB)
    }
}

fn status() -> Result<()> {
    let mut e = Engine::observer(Config::load()?)?;
    e.tick();
    let Some(s) = e.take_snapshot() else {
        println!("no snapshot");
        return Ok(());
    };
    println!("backend : {} ({})", s.backend, s.mechanism);
    if let Some(h) = &s.health {
        println!("WARNING : {h}");
    }
    println!(
        "ram     : {} used of {}",
        human(s.used_ram),
        human(s.total_ram)
    );
    println!(
        "swap    : {} used of {}",
        human(s.used_swap),
        human(s.total_swap)
    );
    println!(
        "game    : {}",
        if s.game_active {
            s.active_games.join(", ")
        } else {
            "none running".into()
        }
    );
    if s.freed_total > 0 {
        println!("freed   : {} so far this session", human(s.freed_total));
    }
    if s.rows.is_empty() {
        println!("\nno matched apps are running");
    } else {
        println!(
            "\n{:<44} {:>10} {:>8} {:>8}  STATE",
            "APP", "ROLE", "RSS", "SWAP"
        );
        for r in &s.rows {
            println!(
                "{:<44} {:>10} {:>8} {:>8}  {}",
                truncate(&r.label, 44),
                r.role.label(),
                human(r.rss),
                human(r.swap),
                r.state
            );
        }
    }
    for line in s.log.iter().rev().take(5).rev() {
        println!("  {line}");
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn list() -> Result<()> {
    let filter = std::env::args().nth(2).unwrap_or_default().to_lowercase();
    let mut e = Engine::observer(Config::load()?)?;
    e.tick();
    let Some(s) = e.take_snapshot() else {
        return Ok(());
    };
    println!("{:<40} {:>8} {:>6}", "PROCESS", "RSS", "COUNT");
    for p in s
        .processes
        .iter()
        .filter(|p| filter.is_empty() || p.name.to_lowercase().contains(&filter))
    {
        if p.rss < MB {
            continue;
        }
        println!(
            "{:<40} {:>8} {:>6}",
            truncate(&p.name, 40),
            human(p.rss),
            p.count
        );
    }
    println!(
        "\nUse these names in the rule lists ({}).",
        Config::path()?.display()
    );
    Ok(())
}

fn release() -> Result<()> {
    let mut e = Engine::new(Config::load()?)?;
    e.shutdown();
    println!("released all ceilings and pins");
    Ok(())
}
