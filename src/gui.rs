//! The desktop UI: pick which processes count as games, which get parked in
//! swap, and which are left strictly alone.

use crate::config::Config;
use crate::engine::{self, Role, Shared, Snapshot};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

const MB: u64 = 1024 * 1024;

fn human(bytes: u64) -> String {
    if bytes >= 1024 * MB {
        format!("{:.1} G", bytes as f64 / (1024.0 * MB as f64))
    } else {
        format!("{} M", bytes / MB)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dashboard,
    Processes,
    Rules,
    Tuning,
    Log,
}

/// Which rule list a picker button targets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Games,
    Victims,
    Exempt,
    NeverSwap,
}

impl ListKind {
    fn title(self) -> &'static str {
        match self {
            ListKind::Games => "Games",
            ListKind::Victims => "Park in swap",
            ListKind::Exempt => "Never touch",
            ListKind::NeverSwap => "Keep in RAM",
        }
    }
    fn help(self) -> &'static str {
        match self {
            ListKind::Games => "While any of these is running, background apps get parked in swap.",
            ListKind::Victims => {
                "Evicted to swap while a game runs, and released the moment you use them again."
            }
            ListKind::Exempt => "Never reclaimed, whatever else matches.",
            ListKind::NeverSwap => {
                "Pinned fully in RAM the whole time ramsquash runs - not just during a game. \
                 For voice chat and anything else that must not stutter. Implies 'Never touch'."
            }
        }
    }
    fn badge(self) -> &'static str {
        match self {
            ListKind::Games => "G",
            ListKind::Victims => "P",
            ListKind::Exempt => "X",
            ListKind::NeverSwap => "R",
        }
    }
}

pub struct App {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    cfg: Config,
    saved: Config,
    tab: Tab,
    search: String,
    adders: [String; 4],
    status: String,
}

impl App {
    fn new(cfg: Config) -> Self {
        let mut app = Self {
            shared: Arc::new(Shared::new(cfg.clone())),
            worker: None,
            saved: cfg.clone(),
            cfg,
            tab: Tab::Dashboard,
            search: String::new(),
            adders: Default::default(),
            status: String::new(),
        };
        if app.cfg.autostart_engine {
            app.start();
        }
        app
    }

    fn list_mut(&mut self, k: ListKind) -> &mut Vec<String> {
        match k {
            ListKind::Games => &mut self.cfg.games,
            ListKind::Victims => &mut self.cfg.victims,
            ListKind::Exempt => &mut self.cfg.exempt,
            ListKind::NeverSwap => &mut self.cfg.never_swap,
        }
    }

    fn running(&self) -> bool {
        self.worker
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    fn start(&mut self) {
        if self.running() {
            return;
        }
        self.push_config();
        self.shared.stop.store(false, Ordering::Relaxed);
        let s = self.shared.clone();
        self.worker = Some(std::thread::spawn(move || engine::run(s)));
        self.status = "engine started".into();
    }

    fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            // Joining matters: the engine lifts every ceiling on its way out,
            // and a half-stopped engine would leave apps capped.
            let _ = h.join();
        }
        self.status = "engine stopped, all holds released".into();
    }

    /// Hand edited rules to the running engine without a restart.
    fn push_config(&mut self) {
        *self.shared.config.lock().unwrap() = self.cfg.clone();
        self.shared.config_dirty.store(true, Ordering::Relaxed);
    }

    fn save(&mut self) {
        self.push_config();
        match self.cfg.save() {
            Ok(p) => {
                self.saved = self.cfg.clone();
                self.status = format!("saved to {}", p.display());
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    fn dirty(&self) -> bool {
        self.cfg != self.saved
    }

    fn add_to(&mut self, k: ListKind, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let list = self.list_mut(k);
        if !list.iter().any(|e| e.eq_ignore_ascii_case(&name)) {
            list.push(name.clone());
            self.push_config();
            self.status = format!("added {name} to {}", k.title());
        }
    }
}

impl eframe::App for App {
    // egui 0.36 hands the app a `Ui` rather than a `Context`; panels are
    // created with the unified `Panel` type and shown inside that `Ui`.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let snap: Snapshot = self.shared.snapshot.lock().unwrap().clone();
        let running = self.running();

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("ramsquash");
                ui.separator();
                if running {
                    if ui.button("⏹ Stop engine").clicked() {
                        self.stop();
                    }
                    ui.colored_label(egui::Color32::from_rgb(90, 200, 120), "● running");
                } else {
                    if ui.button("▶ Start engine").clicked() {
                        self.start();
                    }
                    ui.colored_label(ui.visuals().weak_text_color(), "○ stopped");
                }
                ui.separator();
                let save = ui.add_enabled(self.dirty(), egui::Button::new("💾 Save rules"));
                if save.clicked() {
                    self.save();
                }
                if self.dirty() {
                    ui.colored_label(egui::Color32::from_rgb(230, 180, 80), "unsaved changes");
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Dashboard, "Dashboard");
                ui.selectable_value(&mut self.tab, Tab::Processes, "Processes");
                ui.selectable_value(&mut self.tab, Tab::Rules, "Rules");
                ui.selectable_value(&mut self.tab, Tab::Tuning, "Tuning");
                ui.selectable_value(&mut self.tab, Tab::Log, "Log");
            });
            ui.add_space(2.0);
        });

        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(if snap.backend.is_empty() {
                    "backend: (engine not started)".to_string()
                } else {
                    format!("backend: {}", snap.backend)
                });
                ui.separator();
                ui.label(&self.status);
            });
        });

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Dashboard => self.dashboard(ui, &snap),
            Tab::Processes => self.processes(ui, &snap),
            Tab::Rules => self.rules(ui),
            Tab::Tuning => self.tuning(ui),
            Tab::Log => self.log(ui, &snap),
        });

        if running {
            ctx.request_repaint_after(Duration::from_millis(700));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Never leave the user's apps capped because they closed the window.
        self.stop();
    }
}

impl App {
    fn dashboard(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if let Some(h) = &snap.health {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 100), format!("⚠ {h}"));
            ui.add_space(6.0);
        }
        if !snap.mechanism.is_empty() {
            ui.label(egui::RichText::new(&snap.mechanism).weak().italics());
            ui.add_space(8.0);
        }

        // Before the first tick there is no snapshot, so every figure is zero.
        // Showing "0 M / 0 M" and a "no swap configured" warning would be a lie.
        if snap.total_ram == 0 {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Press ▶ Start engine to see live memory and processes.")
                    .size(15.0),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Nothing is touched until the engine is running, and stopping it \
                     releases everything it was holding.",
                )
                .weak(),
            );
            return;
        }

        egui::Grid::new("meters")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("RAM");
                let frac = if snap.total_ram > 0 {
                    snap.used_ram as f32 / snap.total_ram as f32
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(380.0)
                        .text(format!(
                            "{} / {}",
                            human(snap.used_ram),
                            human(snap.total_ram)
                        )),
                );
                ui.end_row();

                ui.label("Swap");
                let frac = if snap.total_swap > 0 {
                    snap.used_swap as f32 / snap.total_swap as f32
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(380.0)
                        .text(format!(
                            "{} / {}",
                            human(snap.used_swap),
                            human(snap.total_swap)
                        )),
                );
                ui.end_row();
            });

        if snap.total_swap == 0 {
            ui.add_space(4.0);
            ui.colored_label(
                egui::Color32::from_rgb(230, 180, 80),
                "No swap is configured, so there is nowhere to park anything. \
                 Add a swap file (Linux) or leave the pagefile enabled (Windows).",
            );
        }

        ui.add_space(10.0);
        if snap.game_active {
            ui.colored_label(
                egui::Color32::from_rgb(90, 200, 120),
                format!("🎮 game running: {}", snap.active_games.join(", ")),
            );
        } else if snap.running {
            ui.label("no game running - nothing is being parked");
        } else {
            ui.label("engine stopped");
        }
        if snap.freed_total > 0 {
            ui.label(format!(
                "reclaimed so far this session: {}",
                human(snap.freed_total)
            ));
        }

        ui.add_space(12.0);
        ui.separator();
        ui.heading("Managed apps");
        if snap.rows.is_empty() {
            ui.label("Nothing matched yet. Add rules on the Processes or Rules tab.");
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("rows")
                .num_columns(5)
                .striped(true)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    for h in ["App", "Role", "RAM", "In swap", "State"] {
                        ui.label(egui::RichText::new(h).strong());
                    }
                    ui.end_row();
                    for r in &snap.rows {
                        ui.label(&r.label);
                        let c = match r.role {
                            Role::Pinned => egui::Color32::from_rgb(120, 180, 240),
                            Role::Game => egui::Color32::from_rgb(90, 200, 120),
                            _ => ui.visuals().text_color(),
                        };
                        ui.colored_label(c, r.role.label());
                        ui.label(human(r.rss));
                        ui.label(human(r.swap));
                        ui.label(&r.state);
                        ui.end_row();
                    }
                });
        });
    }

    fn processes(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.text_edit_singleline(&mut self.search);
            if ui.button("clear").clicked() {
                self.search.clear();
            }
        });
        ui.label(
            egui::RichText::new(
                "Pick a process, then assign it with the buttons: \
                 G = game, P = park in swap, X = never touch, R = keep in RAM.",
            )
            .weak(),
        );
        if snap.processes.is_empty() {
            ui.add_space(8.0);
            ui.label("Start the engine to see running processes.");
            return;
        }
        ui.add_space(6.0);

        let needle = self.search.to_lowercase();
        let mut pending: Option<(ListKind, String)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("procs")
                .num_columns(4)
                .striped(true)
                .spacing([14.0, 3.0])
                .show(ui, |ui| {
                    for h in ["Process", "RAM", "Count", "Assign to"] {
                        ui.label(egui::RichText::new(h).strong());
                    }
                    ui.end_row();
                    for p in snap
                        .processes
                        .iter()
                        .filter(|p| needle.is_empty() || p.name.to_lowercase().contains(&needle))
                        .filter(|p| p.rss >= MB)
                    {
                        ui.label(&p.name);
                        ui.label(human(p.rss));
                        ui.label(p.count.to_string());
                        ui.horizontal(|ui| {
                            for k in [
                                ListKind::Games,
                                ListKind::Victims,
                                ListKind::Exempt,
                                ListKind::NeverSwap,
                            ] {
                                if ui
                                    .small_button(k.badge())
                                    .on_hover_text(format!("{}: {}", k.title(), k.help()))
                                    .clicked()
                                {
                                    pending = Some((k, p.name.clone()));
                                }
                            }
                        });
                        ui.end_row();
                    }
                });
        });
        // Applied after the loop so we are not mutating config mid-iteration.
        if let Some((k, name)) = pending {
            self.add_to(k, &name);
        }
    }

    fn rules(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, k) in [
                ListKind::Games,
                ListKind::Victims,
                ListKind::Exempt,
                ListKind::NeverSwap,
            ]
            .into_iter()
            .enumerate()
            {
                ui.group(|ui| {
                    ui.label(egui::RichText::new(k.title()).strong().size(15.0));
                    ui.label(egui::RichText::new(k.help()).weak());
                    ui.add_space(4.0);

                    let mut remove: Option<usize> = None;
                    let entries = self.list_mut(k).clone();
                    if entries.is_empty() {
                        ui.label(egui::RichText::new("(empty)").weak().italics());
                    }
                    ui.horizontal_wrapped(|ui| {
                        for (idx, e) in entries.iter().enumerate() {
                            if ui
                                .button(format!("{e}  ✕"))
                                .on_hover_text("click to remove")
                                .clicked()
                            {
                                remove = Some(idx);
                            }
                        }
                    });
                    if let Some(idx) = remove {
                        self.list_mut(k).remove(idx);
                        self.push_config();
                    }

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.adders[i])
                                .hint_text("process name, * allowed")
                                .desired_width(240.0),
                        );
                        let submit =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.button("Add").clicked() || submit {
                            let v = self.adders[i].clone();
                            self.add_to(k, &v);
                            self.adders[i].clear();
                        }
                    });
                });
                ui.add_space(8.0);
            }
        });
    }

    fn tuning(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut changed = false;
            let row = |ui: &mut egui::Ui, label: &str, help: &str, w: egui::Slider<'_>| {
                ui.label(egui::RichText::new(label).strong());
                let r = ui.add(w);
                ui.label(egui::RichText::new(help).weak().small());
                ui.add_space(8.0);
                r.changed()
            };

            changed |= row(
                ui,
                "Game must use at least",
                "Stops launchers and leftover helpers from counting as a running game.",
                egui::Slider::new(&mut self.cfg.min_game_mb, 50..=4096).suffix(" MB"),
            );
            changed |= row(
                ui,
                "Ceiling headroom",
                "A parked app is capped at its post-eviction size plus this. Too small and it stalls.",
                egui::Slider::new(&mut self.cfg.headroom_mb, 32..=1024).suffix(" MB"),
            );
            changed |= row(
                ui,
                "Check interval",
                "How often to look for games and sample the managed apps.",
                egui::Slider::new(&mut self.cfg.tick_secs, 1..=30).suffix(" s"),
            );
            changed |= row(
                ui,
                "Idle before re-parking",
                "How long a woken app must sit unused before it is parked again.",
                egui::Slider::new(&mut self.cfg.idle_grace_secs, 10..=600).suffix(" s"),
            );
            changed |= row(
                ui,
                "Settle time after parking",
                "Activity is ignored this long after parking: eviction's own cost otherwise \
                 looks like use and wakes the app straight back up.",
                egui::Slider::new(&mut self.cfg.settle_secs, 1..=30).suffix(" s"),
            );
            changed |= row(
                ui,
                "Wake on CPU above",
                "An app using this much of one core counts as in use.",
                egui::Slider::new(&mut self.cfg.wake_cpu_pct, 1.0..=100.0).suffix(" %"),
            );
            changed |= row(
                ui,
                "Wake on memory pressure above",
                "Escape hatch: an app thrashing against its ceiling can be too stuck to look \
                 busy any other way. Linux only.",
                egui::Slider::new(&mut self.cfg.stall_pct, 1.0..=90.0).suffix(" %"),
            );

            ui.separator();
            changed |= ui
                .checkbox(
                    &mut self.cfg.pin_game_in_ram,
                    "Keep the game itself out of swap while it runs",
                )
                .on_hover_text(
                    "Avoids page-in hitches mid-match. Trade-off: in a real memory emergency \
                     the system must kill rather than page the game out.",
                )
                .changed();
            changed |= ui
                .checkbox(&mut self.cfg.autostart_engine, "Start the engine when this app opens")
                .changed();

            if changed {
                self.push_config();
            }
        });
    }

    fn log(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if snap.log.is_empty() {
            ui.label("Nothing logged yet.");
            return;
        }
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &snap.log {
                    ui.label(egui::RichText::new(line).monospace().small());
                }
            });
    }
}

pub fn run() -> Result<()> {
    let cfg = Config::load().unwrap_or_default();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("ramsquash"),
        ..Default::default()
    };
    eframe::run_native(
        "ramsquash",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(cfg)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI failed to start: {e}"))
}
