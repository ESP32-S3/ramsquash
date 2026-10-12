# ramsquash

Keep a game's RAM free by parking idle background apps in swap — and pull them
straight back the moment you use them again.

Runs on **Linux** and **Windows**. Single binary, no runtime to install.

```
┌─ game starts ──► background apps evicted to swap, held under a ceiling
│                  the game itself pinned in RAM so it never hitches
├─ you alt-tab ──► that app is released instantly and pages back on demand
├─ it goes idle ─► parked again
└─ game exits ───► every ceiling lifted
```

## Why not just set swappiness?

Swappiness is global and advisory: you ask the kernel to lean harder on swap
and hope the right pages go. ramsquash is targeted — it names an app and
evicts *that* app, right now, leaving the game untouched.

## How it works

|                   | Linux                        | Windows                               |
| ----------------- | ---------------------------- | ------------------------------------- |
| Evict now         | `memory.reclaim` (cgroup v2) | `EmptyWorkingSet`                     |
| Hold a ceiling    | `memory.high`                | hard working-set max                  |
| Pin fully in RAM  | `memory.swap.max = 0`        | hard working-set min                  |
| "App is in use"   | `memory.events` high count   | hard page-fault count                 |
| Thrash escape     | PSI `memory.pressure`        | *(not available)*                     |
| Privileges        | none, where systemd delegates the memory controller | same-user processes; elevated ones need admin |

The release signal is the interesting part. Rather than trying to track window
focus — which is unavailable to unprivileged apps on Wayland, and fiddly
everywhere — ramsquash watches the app's own behaviour. Every time a parked app
is throttled against its ceiling, a counter ticks. A rising counter means it is
faulting its working set back in, which is precisely what happens the instant
you switch to it. The ceiling lifts and it pages back at SSD speed, touched
pages only.

## The four lists

| List              | Meaning                                                                  |
| ----------------- | ------------------------------------------------------------------------ |
| **Games**         | While one of these runs, parking is active.                               |
| **Park in swap**  | Evicted while a game runs; released as soon as you use them.              |
| **Never touch**   | Never reclaimed, whatever else matches.                                   |
| **Keep in RAM**   | Pinned the *entire* time ramsquash runs — not just during a game.         |

"Keep in RAM" exists for voice chat. Discord accumulates hundreds of MB in the
pagefile during ordinary desktop use, with no game involved; being paged out
mid-call is what makes voice crackle. Pinning it is unconditional, so a call is
safe whether or not you happen to be in a game. Discord and the usual
conferencing apps are on both default lists.

Names are matched case-insensitively against the process name and its
executable, with `*` and `?` wildcards — so `firefox` matches `firefox.exe`,
and `steam_app_*` matches them all.

## Install

Grab a binary from [Releases](../../releases), or build it:

```bash
cargo build --release          # with GUI
cargo build --release --no-default-features   # headless: daemon + CLI only
```

### Linux: check you have somewhere to park things

ramsquash can only move pages to swap that already exists. If `swapon --show`
is empty or tiny, add a swap file:

```bash
sudo fallocate -l 16G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

An NVMe also likes `vm.page-cluster=1` (lower swap-in latency than the
rotational-disk default of 3).

Run it as a service with [`packaging/ramsquash.service`](packaging/ramsquash.service).

### Windows

Leave the pagefile on automatic. ramsquash works on processes you own; to
manage an elevated app, run it elevated too.

## Usage

```
ramsquash            open the GUI
ramsquash daemon     run the engine in the foreground (for a service)
ramsquash status     print current state once
ramsquash list       list running processes by memory use, for writing rules
ramsquash release    lift every ceiling and pin
ramsquash config     print the config file path
```

Config is TOML, at `~/.config/ramsquash/config.toml` or
`%APPDATA%\ramsquash\config.toml`. The GUI writes it for you.

## Safety

Parking an app is reversible and costs it a page-in, nothing more. Even so:

- Every ceiling and pin is written to a **durable hold file**. A `SIGKILL` or a
  power cut would otherwise leave your apps capped with nothing running that
  knows how to undo it; on next start ramsquash restores everything first.
- Closing the window, stopping the service, and `Ctrl-C` all release holds.
- ramsquash refuses to act on its own process group.
- Apps seen for the first time mid-game get a grace period instead of being
  parked immediately — if it just appeared, you are probably about to use it.
- A parked app that ends up **thrashing** against its ceiling is force-released.
  Without that escape hatch an app can get too stuck to look busy by any other
  measure, and stays pinned at its ceiling indefinitely. This one is a real
  failure mode, observed and fixed during development.

### Trade-off worth knowing

"Keep the game out of swap" (on by default) means the kernel cannot page the
game out at all. Frametimes stay clean, but in a genuine memory emergency the
system must kill something rather than page the game out. Turn it off in
**Tuning** if you run close to the edge.

## Building and testing

```bash
cargo test
cargo clippy --all-targets
cargo check --target x86_64-pc-windows-msvc   # type-check Windows from Linux
```

## License

MIT
