//! dropin manager: the main window. Lists installed apps and exposes every
//! operation the CLI has — update, rollback, remove, GitHub tracking,
//! doctor — so the terminal is never needed.
//!
//! Long operations run on background threads; the UI polls them and
//! refreshes the app list when they finish.

use crate::style;
use dropin_core::{health, install, integrate, settings};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// One row in the app list.
#[derive(Clone)]
struct AppRow {
    name: String,
    format: String,
    version: Option<String>,
    verified: bool,
    github_repo: Option<String>,
    has_snapshots: bool,
}

fn load_apps() -> Vec<AppRow> {
    let mut rows = Vec::new();
    if let Ok(list) = install::list() {
        for inst in list {
            let has_snapshots = install::can_rollback(&inst.name);
            rows.push(AppRow {
                name: inst.name,
                format: inst.format,
                version: inst.version,
                verified: inst.verified,
                github_repo: inst.github_repo,
                has_snapshots,
            });
        }
    }
    rows
}

/// Result of a background operation.
struct OpDone {
    message: String,
    refresh_apps: bool,
}

struct BusyOp {
    label: String,
    rx: Receiver<OpDone>,
}

pub struct ManagerApp {
    apps: Vec<AppRow>,
    status: String,
    busy: Option<BusyOp>,
    // dialogs
    track_name: Option<String>,
    track_input: String,
    remove_name: Option<String>,
    remove_purge: bool,
    rollback_name: Option<String>,
    doctor_open: bool,
    doctor_issues: Vec<health::HealthIssue>,
    install_children: Vec<std::process::Child>,
    /// First-run: offer one-click desktop integration.
    show_welcome: bool,
    settings: settings::Settings,
    settings_open: bool,
    auto_checked: bool,
}

impl ManagerApp {
    pub fn new() -> Self {
        Self {
            apps: load_apps(),
            status: String::new(),
            busy: None,
            track_name: None,
            track_input: String::new(),
            remove_name: None,
            remove_purge: false,
            rollback_name: None,
            doctor_open: false,
            doctor_issues: Vec::new(),
            install_children: Vec::new(),
            show_welcome: !integrate::is_integrated(),
            settings: settings::load(),
            settings_open: false,
            auto_checked: false,
        }
    }

    /// Background update check that also pops a desktop notification when
    /// something can be updated.
    fn auto_check(&mut self) {
        self.auto_checked = true;
        if self.apps.is_empty() {
            return;
        }
        self.run_op("Checking for updates", || {
            let (message, updates) = match install::update(None, true) {
                Ok(reports) => {
                    let n = reports
                        .iter()
                        .filter(|r| {
                            matches!(r.status, install::UpdateStatus::Updated(_))
                        })
                        .count();
                    (fmt_reports(reports), n)
                }
                Err(e) => (format!("Update check failed: {e}"), 0),
            };
            if updates > 0 {
                let _ = notify_rust::Notification::new()
                    .summary("dropin")
                    .body(&format!(
                        "{updates} app{} updated.",
                        if updates == 1 { "" } else { "s" }
                    ))
                    .show();
            }
            OpDone { message, refresh_apps: true }
        });
    }

    /// Run `f` on a background thread; the UI stays responsive.
    fn run_op(&mut self, label: &str, f: impl FnOnce() -> OpDone + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let done = f();
            let _ = tx.send(done);
        });
        self.busy = Some(BusyOp { label: label.to_string(), rx });
        self.status = format!("{label}…");
    }

    fn refresh(&mut self) {
        self.apps = load_apps();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        // Install dialogs spawned from here: refresh when any exits.
        let mut exited = false;
        self.install_children.retain_mut(|child| match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                false
            }
            Ok(None) => true,
            Err(_) => {
                exited = true;
                false
            }
        });
        if exited {
            self.refresh();
            self.status = "App list refreshed.".to_string();
        }
        if let Some(op) = &self.busy {
            if let Ok(done) = op.rx.try_recv() {
                if done.refresh_apps {
                    self.refresh();
                }
                self.status = done.message;
                self.busy = None;
            } else {
                ctx.request_repaint();
            }
        }
    }
}

fn fmt_reports(reports: Vec<install::UpdateReport>) -> String {
    if reports.is_empty() {
        return "Nothing to update.".to_string();
    }
    let mut lines = Vec::new();
    for r in reports {
        let (tag, msg) = match r.status {
            install::UpdateStatus::UpToDate(m) => ("up to date", m),
            install::UpdateStatus::Updated(m) => ("updated", m),
            install::UpdateStatus::Manual(m) => ("manual", m),
            install::UpdateStatus::Failed(m) => ("failed", m),
        };
        lines.push(format!("{}: [{tag}] {msg}", r.name));
    }
    lines.join("\n")
}

impl eframe::App for ManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);

        // Drag-and-drop: files dropped on the window open install dialogs.
        let dropped: Vec<PathBuf> = ctx
            .input(|i| i.raw.dropped_files.clone())
            .into_iter()
            .filter_map(|f| f.path)
            .collect();
        for path in dropped {
            self.spawn_installer(&path);
        }

        // Automatic update check on launch (once, in the background).
        if !self.auto_checked && self.settings.auto_check_updates {
            self.auto_check();
        }

        let busy = self.busy.is_some();

        // ---- toolbar ----
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("dropin").size(20.0).strong());
                ui.add_space(16.0);
                if style::primary_button_enabled(ui, !busy, "Install app…").clicked() && !busy {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Choose a file to install")
                        .pick_file()
                    {
                        self.spawn_installer(&path);
                    }
                }
                if ui.button("Check for updates").clicked() && !busy {
                    self.run_op("Checking for updates", || {
                        let msg = match install::update(None, true) {
                            Ok(reports) => fmt_reports(reports),
                            Err(e) => format!("Update check failed: {e}"),
                        };
                        OpDone { message: msg, refresh_apps: true }
                    });
                }
                if style::ghost_button(ui, "Doctor").clicked() && !busy {
                    self.doctor_issues = health::check();
                    self.doctor_open = true;
                }
                if style::ghost_button(ui, "Settings").clicked() {
                    self.settings_open = true;
                }
            });
            ui.add_space(4.0);
        });

        // ---- status bar ----
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if let Some(op) = &self.busy {
                    ui.spinner();
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(format!("{}…", op.label)).color(style::color::TEXT_DIM));
                } else if self.status.is_empty() {
                    style::faint_label(ui, "Ready");
                } else {
                    style::dim_label(ui, &self.status);
                }
            });
            ui.add_space(2.0);
        });

        // ---- app list ----
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            if self.apps.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.label(egui::RichText::new("No apps installed yet").size(18.0).strong());
                    ui.add_space(8.0);
                    style::dim_label(ui, "Click “Install app…” above, or double-click any installer file.");
                    ui.add_space(8.0);
                    style::faint_label(ui, "You can also drag and drop files onto this window.");
                });
                return;
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} app{} installed",
                    self.apps.len(),
                    if self.apps.len() == 1 { "" } else { "s" }
                ))
                .small()
                .color(style::color::TEXT_FAINT),
            );
            ui.add_space(8.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                for i in 0..self.apps.len() {
                    // Clone the row: app_card may mutate self (dialogs, ops).
                    let app = self.apps[i].clone();
                    self.app_card(ui, &app, busy);
                    ui.add_space(8.0);
                }
            });
        });

        self.dialogs(ctx, busy);
    }
}

impl ManagerApp {
    fn spawn_installer(&mut self, path: &PathBuf) {
        match std::env::current_exe() {
            Ok(exe) => match std::process::Command::new(exe).arg(path).spawn() {
                Ok(child) => {
                    self.install_children.push(child);
                    self.status = "Install dialog opened.".to_string();
                }
                Err(e) => self.status = format!("Couldn't open installer: {e}"),
            },
            Err(e) => self.status = format!("Couldn't open installer: {e}"),
        }
    }

    fn app_card(&mut self, ui: &mut egui::Ui, app: &AppRow, busy: bool) {
        // Actions decided while rendering, run after.
        enum Act {
            None,
            Update,
            Rollback,
            Remove,
            Track,
            Untrack,
        }
        let mut act = Act::None;

        style::card_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                style::app_badge(ui, &app.name, 48.0);
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&app.name).size(17.0).strong());
                        if app.verified {
                            ui.label(
                                egui::RichText::new("✓")
                                    .color(style::color::GOOD)
                                    .strong(),
                            );
                        }
                    });
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        style::badge(
                            ui,
                            &app.format,
                            style::format_color(&app.format),
                        );
                        if let Some(v) = &app.version {
                            style::badge(ui, &format!("v{v}"), style::color::BORDER);
                        }
                    });
                    if let Some(repo) = &app.github_repo {
                        ui.add_space(2.0);
                        style::faint_label(ui, &format!("tracking {repo}"));
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if style::ghost_button(ui, "Remove").clicked() && !busy {
                        act = Act::Remove;
                    }
                    if app.has_snapshots && style::ghost_button(ui, "Rollback").clicked() && !busy {
                        act = Act::Rollback;
                    }
                    // Script installs have no update channel or GitHub tracking;
                    // hiding those buttons avoids dead ends.
                    let is_script = app.format == "installer script";
                    if !is_script && ui.button("Update").clicked() && !busy {
                        act = Act::Update;
                    }
                    if !is_script {
                        if app.github_repo.is_some() {
                            if style::ghost_button(ui, "Untrack").clicked() && !busy {
                                act = Act::Untrack;
                            }
                        } else if style::ghost_button(ui, "Track…").clicked() && !busy {
                            act = Act::Track;
                        }
                    }
                });
            });
        });

        let name = app.name.clone();
        match act {
            Act::None => {}
            Act::Update => self.run_op(&format!("Updating {name}"), move || {
                let msg = match install::update(Some(&name), false) {
                    Ok(reports) => fmt_reports(reports),
                    Err(e) => format!("Update failed: {e}"),
                };
                OpDone { message: msg, refresh_apps: true }
            }),
            Act::Rollback => self.rollback_name = Some(name),
            Act::Remove => {
                self.remove_name = Some(name);
                self.remove_purge = false;
            }
            Act::Track => {
                self.track_name = Some(name);
                self.track_input.clear();
            }
            Act::Untrack => self.run_op(&format!("Untracking {name}"), move || {
                let message = install::untrack(&name)
                    .map(|m| format!("{m}."))
                    .unwrap_or_else(|e| format!("Couldn't untrack: {e}"));
                OpDone { message, refresh_apps: true }
            }),
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context, busy: bool) {
        // ---- first-run welcome / one-click setup ----
        if self.show_welcome {
            let mut open = true;
            let mut setup = false;
            let mut close = false;
            egui::Window::new("Welcome to dropin")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Download it. Double-click it. Done.");
                    ui.add_space(8.0);
                    ui.label("Set up dropin so double-clicking any installer file\njust works, and apps get menu entries?");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!busy, |ui| {
                            if ui.button("Set up dropin").clicked() {
                                setup = true;
                            }
                        });
                        if ui.button("Later").clicked() {
                            close = true;
                        }
                    });
                });
            if close {
                open = false;
            }
            if setup {
                self.run_op("Setting up", || {
                    let message = integrate::integrate(true)
                        .map(|_| "dropin is set up — double-click any installer file.".to_string())
                        .unwrap_or_else(|e| format!("Setup failed: {e}"));
                    OpDone { message, refresh_apps: false }
                });
                open = false;
            }
            if !open {
                self.show_welcome = false;
            }
        }

        // ---- track dialog ----
        if let Some(name) = self.track_name.clone() {
            let mut open = true;
            let mut submit = false;
            let mut close = false;
            egui::Window::new(format!("Track GitHub releases for {name}"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    style::dim_label(ui, "GitHub repo (owner/repo):");
                    ui.add_space(4.0);
                    ui.text_edit_singleline(&mut self.track_input);
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if style::ghost_button(ui, "Cancel").clicked() {
                                close = true;
                            }
                            ui.add_enabled_ui(!busy, |ui| {
                                if style::primary_button(ui, "Track").clicked() {
                                    submit = true;
                                }
                            });
                        });
                    });
                });
            if close {
                open = false;
            }
            if submit {
                let repo = self.track_input.trim().to_string();
                self.run_op(&format!("Tracking {name}"), move || {
                    let message = install::track(&name, &repo)
                        .map(|m| format!("{m}."))
                        .unwrap_or_else(|e| format!("Couldn't track: {e}"));
                    OpDone { message, refresh_apps: true }
                });
                open = false;
            }
            if !open {
                self.track_name = None;
            }
        }

        // ---- remove confirm ----
        if let Some(name) = self.remove_name.clone() {
            let mut open = true;
            let mut confirm = false;
            let mut close = false;
            egui::Window::new(format!("Remove {name}?"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new(format!("Remove {name} and undo its installation?"))
                            .size(15.0),
                    );
                    ui.add_space(6.0);
                    ui.checkbox(&mut self.remove_purge, "Also move leftover config/cache/data to the trash");
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if style::ghost_button(ui, "Cancel").clicked() {
                                close = true;
                            }
                            ui.add_enabled_ui(!busy, |ui| {
                                let btn = egui::Button::new(
                                    egui::RichText::new("Remove").strong().color(style::color::TEXT),
                                )
                                .fill(style::color::BAD)
                                .corner_radius(8.0)
                                .min_size(egui::vec2(100.0, 34.0));
                                if ui.add(btn).clicked() {
                                    confirm = true;
                                }
                            });
                        });
                    });
                });
            if close {
                open = false;
            }
            if confirm {
                let purge = self.remove_purge;
                self.run_op(&format!("Removing {name}"), move || {
                    let message = install::remove(&name, purge)
                        .unwrap_or_else(|e| format!("Couldn't remove: {e}"));
                    OpDone { message, refresh_apps: true }
                });
                open = false;
            }
            if !open {
                self.remove_name = None;
            }
        }

        // ---- rollback confirm ----
        if let Some(name) = self.rollback_name.clone() {
            let mut open = true;
            let mut confirm = false;
            let mut close = false;
            egui::Window::new(format!("Roll back {name}?"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new("Restore the version from before the last update?")
                            .size(15.0),
                    );
                    ui.add_space(4.0);
                    style::dim_label(
                        ui,
                        "The current version is kept, so you can switch back again.",
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if style::ghost_button(ui, "Cancel").clicked() {
                                close = true;
                            }
                            ui.add_enabled_ui(!busy, |ui| {
                                if style::primary_button(ui, "Roll back").clicked() {
                                    confirm = true;
                                }
                            });
                        });
                    });
                });
            if close {
                open = false;
            }
            if confirm {
                self.run_op(&format!("Rolling back {name}"), move || {
                    let message = install::rollback(&name)
                        .map(|m| format!("{m}."))
                        .unwrap_or_else(|e| format!("Couldn't roll back: {e}"));
                    OpDone { message, refresh_apps: true }
                });
                open = false;
            }
            if !open {
                self.rollback_name = None;
            }
        }

        // ---- settings ----
        if self.settings_open {
            let mut open = true;
            egui::Window::new("Settings")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    if ui
                        .checkbox(&mut self.settings.auto_check_updates, "Check for updates when dropin opens")
                        .changed()
                    {
                        let _ = settings::save(&self.settings);
                    }
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("dropin never phones home on its own —\nchecks only run when you open the manager.").weak());
                });
            if !open {
                self.settings_open = false;
            }
        }

        // ---- doctor results ----
        if self.doctor_open {
            let mut open = true;
            egui::Window::new("Doctor")
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    if self.doctor_issues.is_empty() {
                        ui.label("No issues found. Everything looks healthy.");
                    } else {
                        egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                            for issue in &self.doctor_issues {
                                let tag = match issue.kind {
                                    health::HealthKind::Orphaned => "orphaned",
                                    health::HealthKind::Duplicate => "duplicate",
                                    health::HealthKind::Shadowed => "shadowed",
                                    health::HealthKind::CrossSource => "also-installed",
                                    health::HealthKind::BrokenExec => "broken-menu",
                                    health::HealthKind::NoFuse => "no-fuse",
                                };
                                ui.label(egui::RichText::new(format!("[{tag}] {}", issue.message)).strong());
                                ui.label(egui::RichText::new(format!("hint: {}", issue.hint)).weak());
                                ui.add_space(6.0);
                            }
                        });
                    }
                });
            if !open {
                self.doctor_open = false;
            }
        }
    }
}
