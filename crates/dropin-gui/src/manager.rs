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
    has_env: bool,
    /// Human-readable disk usage, if the install root exists.
    size: Option<String>,
    /// Install date as YYYY-MM-DD, if known.
    installed_on: Option<String>,
    /// Where the app is installed, if known.
    install_root: Option<String>,
    /// Original source file, if known.
    source_path: Option<String>,
    /// Env vars as (key, value) pairs.
    env_vars: Vec<(String, String)>,
    /// True when an update is available (from background check).
    has_update: bool,
    /// True when the app was updated in the last check.
    recently_updated: bool,
    /// App category (Games, Media, Utilities, etc.).
    category: String,
}

/// Recursively sum file sizes under a path (cap at 100k entries).
fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut count = 0u32;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        if count > 100_000 {
            break;
        }
        count += 1;
        if let Ok(meta) = std::fs::symlink_metadata(&p) {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                if let Ok(entries) = std::fs::read_dir(&p) {
                    for e in entries.filter_map(|e| e.ok()) {
                        stack.push(e.path());
                    }
                }
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Check if dropin is set to launch at startup.
fn is_autostart_enabled() -> bool {
    dropin_core::home_dir()
        .join(".config/autostart/dropin.desktop")
        .exists()
}

/// Get the category for an app from its .desktop file.
/// Returns a simplified category: Game, Utility, Media, etc.
fn app_category(slug: &str, format: &str) -> String {
    // Wine/Proton games.
    if format.contains("Windows") {
        // Check if it's a game via the desktop file.
        let desktop_path = dropin_core::home_dir()
            .join(".local/share/applications")
            .join(format!("dropin-{slug}.desktop"));
        if let Ok(text) = std::fs::read_to_string(&desktop_path) {
            if text.contains("Categories=Game") {
                return "Games".to_string();
            }
        }
        return "Windows Apps".to_string();
    }
    // Parse .desktop Categories.
    let desktop_path = dropin_core::home_dir()
        .join(".local/share/applications")
        .join(format!("dropin-{slug}.desktop"));
    if let Ok(text) = std::fs::read_to_string(&desktop_path) {
        for line in text.lines() {
            if let Some(cats) = line.strip_prefix("Categories=") {
                let cats = cats.to_lowercase();
                if cats.contains("game") {
                    return "Games".to_string();
                }
                if cats.contains("audio") || cats.contains("video") || cats.contains("media") {
                    return "Media".to_string();
                }
                if cats.contains("development") || cats.contains("programming") {
                    return "Development".to_string();
                }
                if cats.contains("graphics") || cats.contains("photography") {
                    return "Graphics".to_string();
                }
                if cats.contains("network") || cats.contains("webbrowser") {
                    return "Internet".to_string();
                }
                if cats.contains("office") || cats.contains("productivity") {
                    return "Productivity".to_string();
                }
                if cats.contains("utility") || cats.contains("system") {
                    return "Utilities".to_string();
                }
            }
        }
    }
    // Fallback by format.
    match format {
        f if f.contains("AppImage") => "Applications".to_string(),
        f if f.contains("Flatpak") => "Applications".to_string(),
        _ => "Other".to_string(),
    }
}

/// Enable or disable launch at startup.
fn set_autostart(enabled: bool) {
    let path = dropin_core::home_dir().join(".config/autostart/dropin.desktop");
    if enabled {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let content = format!(
                "[Desktop Entry]\nType=Application\nName=dropin\nExec={}\nHidden=false\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            );
            let _ = std::fs::write(&path, content);
        }
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

fn load_apps(
    available: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    recent: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> Vec<AppRow> {
    let mut rows = Vec::new();
    if let Ok(list) = install::list() {
        for inst in list {
            let has_snapshots = install::can_rollback(&inst.name);
            let has_env = inst
                .env_vars
                .as_ref()
                .map(|j| j != "{}" && !j.is_empty())
                .unwrap_or(false);
            // Disk usage from the install root.
            let size = inst.install_root.as_ref().and_then(|root| {
                let p = std::path::Path::new(root);
                if p.exists() {
                    Some(human_size(dir_size(p)))
                } else {
                    None
                }
            });
            // Install date from created_at (first 10 chars = YYYY-MM-DD).
            let installed_on = if inst.created_at.len() >= 10 {
                Some(inst.created_at[..10].to_string())
            } else {
                None
            };
            rows.push(AppRow {
                name: inst.name.clone(),
                format: inst.format.clone(),
                version: inst.version,
                verified: inst.verified,
                github_repo: inst.github_repo,
                has_snapshots,
                has_env,
                size,
                installed_on,
                install_root: inst.install_root,
                source_path: Some(inst.source_path),
                env_vars: inst
                    .env_vars
                    .as_ref()
                    .and_then(|j| {
                        serde_json::from_str::<std::collections::BTreeMap<String, String>>(j).ok()
                    })
                    .map(|m| m.into_iter().collect())
                    .unwrap_or_default(),
                has_update: available
                    .lock()
                    .map(|v| v.contains(&inst.name))
                    .unwrap_or(false),
                recently_updated: recent
                    .lock()
                    .map(|v| v.contains(&inst.name))
                    .unwrap_or(false),
                category: app_category(&inst.name, &inst.format),
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
    // env var editor
    env_name: Option<String>,
    env_vars: Vec<(String, String)>,
    env_new_key: String,
    env_new_value: String,
    // search/filter
    search: String,
    sort_by: SortBy,
    format_filter: Option<String>,
    category_filter: Option<String>,
    // bulk selection
    selected: std::collections::BTreeSet<String>,
    select_mode: bool,
    details_name: Option<String>,
    /// Apps with available updates (shared with background thread).
    available_updates: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Apps updated in the last check (shared with background thread).
    recently_updated: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SortBy {
    #[default]
    Name,
    Size,
    Recent,
}

impl SortBy {
    fn label(self) -> &'static str {
        match self {
            SortBy::Name => "Name",
            SortBy::Size => "Size",
            SortBy::Recent => "Recently installed",
        }
    }
}

impl ManagerApp {
    pub fn new() -> Self {
        let available_updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recently_updated = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        Self {
            apps: load_apps(&available_updates, &recently_updated),
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
            env_name: None,
            env_vars: Vec::new(),
            env_new_key: String::new(),
            env_new_value: String::new(),
            search: String::new(),
            sort_by: SortBy::Name,
            format_filter: None,
            category_filter: None,
            select_mode: false,
            details_name: None,
            available_updates,
            recently_updated,
            selected: std::collections::BTreeSet::new(),
        }
    }

    fn load_env_vars(&self) -> Vec<(String, String)> {
        let name = match &self.env_name {
            Some(n) => n,
            None => return Vec::new(),
        };
        let ledger = match dropin_core::ledger::Ledger::open() {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        ledger
            .env_vars(&dropin_core::slugify(name))
            .map(|m| m.into_iter().collect())
            .unwrap_or_default()
    }

    fn save_env_vars(&self) {
        let name = match &self.env_name {
            Some(n) => n.clone(),
            None => return,
        };
        let map: std::collections::BTreeMap<String, String> =
            self.env_vars.iter().cloned().collect();
        if let Ok(json) = serde_json::to_string(&map) {
            if let Ok(ledger) = dropin_core::ledger::Ledger::open() {
                let _ = ledger.set_env_vars(&dropin_core::slugify(&name), &json);
            }
        }
    }

    /// Background update check that also pops a desktop notification when
    /// something can be updated.
    fn auto_check(&mut self) {
        self.auto_checked = true;
        if self.apps.is_empty() {
            return;
        }
        let available_arc = self.available_updates.clone();
        let recent_arc = self.recently_updated.clone();
        self.run_op("Checking for updates", move || {
            // Check for available updates first (for badges).
            let available = install::check_for_updates().unwrap_or_default();
            if let Ok(mut v) = available_arc.lock() {
                *v = available;
            }
            let (message, updated_names) = match install::update(None, true) {
                Ok(reports) => {
                    let names: Vec<String> = reports
                        .iter()
                        .filter(|r| matches!(r.status, install::UpdateStatus::Updated(_)))
                        .map(|r| r.name.clone())
                        .collect();
                    (fmt_reports(reports), names)
                }
                Err(e) => (format!("Update check failed: {e}"), Vec::new()),
            };
            let updates = updated_names.len();
            // Mark recently updated for badge display.
            if let Ok(mut v) = recent_arc.lock() {
                *v = updated_names.clone();
            }
            // Clear the "available" badge for apps that were just updated.
            if let Ok(mut v) = available_arc.lock() {
                v.retain(|n| !updated_names.contains(n));
            }
            if updates > 0 {
                let _ = notify_rust::Notification::new()
                    .summary("dropin")
                    .body(&format!(
                        "{updates} app{} updated.",
                        if updates == 1 { "" } else { "s" }
                    ))
                    .show();
            }
            OpDone {
                message,
                refresh_apps: true,
            }
        });
    }

    /// Run `f` on a background thread; the UI stays responsive.
    fn run_op(&mut self, label: &str, f: impl FnOnce() -> OpDone + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let done = f();
            let _ = tx.send(done);
        });
        self.busy = Some(BusyOp {
            label: label.to_string(),
            rx,
        });
        self.status = format!("{label}…");
    }

    fn refresh(&mut self) {
        self.apps = load_apps(&self.available_updates, &self.recently_updated);
    }

    fn poll(&mut self, ctx: &egui::Context) {
        // Install dialogs spawned from here: refresh when any exits.
        let mut exited = false;
        self.install_children
            .retain_mut(|child| match child.try_wait() {
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

        // Keyboard shortcuts: Ctrl+F focuses search, Escape clears it.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F)) {
            ctx.memory_mut(|m| {
                m.request_focus(egui::Id::new("search_field"));
            });
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.search.is_empty() {
            self.search.clear();
            self.selected.clear();
        }

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
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                // Brand: distinctive, with room to breathe.
                ui.label(
                    egui::RichText::new("dropin")
                        .size(24.0)
                        .strong()
                        .color(style::color::TEXT),
                );
                ui.add_space(32.0);
                // Primary actions.
                if style::primary_button_enabled(ui, !busy, "Install app…").clicked() && !busy {
                    if let Some(paths) = rfd::FileDialog::new()
                        .set_title("Choose files to install")
                        .pick_files()
                    {
                        for path in paths {
                            self.spawn_installer(&path);
                        }
                    }
                }
                ui.add_space(4.0);
                if style::secondary_button(ui, "Update all").clicked() && !busy {
                    self.run_op("Updating apps", || {
                        let msg = match install::update(None, true) {
                            Ok(reports) => fmt_reports(reports),
                            Err(e) => format!("Update check failed: {e}"),
                        };
                        OpDone {
                            message: msg,
                            refresh_apps: true,
                        }
                    });
                }
                // Right side: search and tools menu.
                // Reserve width so the two sides never collide.
                let right_width = 300.0;
                let avail = ui.available_width();
                if avail > right_width {
                    ui.add_space(avail - right_width);
                }
                ui.horizontal(|ui| {
                    let resp =
                        style::search_field(ui, &mut self.search, egui::Id::new("search_field"));
                    if resp.changed() {
                        self.selected.clear();
                    }
                    ui.add_space(8.0);
                    style::tools_menu_button(ui, |ui| {
                        ui.set_min_width(180.0);
                        if ui.button("Doctor").clicked() && !busy {
                            self.doctor_issues = health::check();
                            self.doctor_open = true;
                            ui.close();
                        }
                        if ui.button("Settings").clicked() {
                            self.settings_open = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Export app list").clicked() && !busy {
                            if let Some(path) = rfd::FileDialog::new()
                                .set_title("Export app list")
                                .set_file_name("dropin-apps.json")
                                .save_file()
                            {
                                if let Ok(apps) = install::list() {
                                    if let Ok(json) = serde_json::to_string_pretty(&apps) {
                                        if std::fs::write(&path, json).is_ok() {
                                            self.status = format!(
                                                "Exported {} apps to {}.",
                                                apps.len(),
                                                path.display()
                                            );
                                        }
                                    }
                                }
                            }
                            ui.close();
                        }
                        if ui.button("Import app list").clicked() && !busy {
                            if let Some(path) = rfd::FileDialog::new()
                                .set_title("Import app list")
                                .pick_file()
                            {
                                self.status = format!(
                                    "Import from {}: reinstall apps from their original files.",
                                    path.display()
                                );
                            }
                            ui.close();
                        }
                    });
                });
            });
            ui.add_space(8.0);
            // Bulk actions bar, only when something is selected.
            if !self.selected.is_empty() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{} selected", self.selected.len()))
                            .small()
                            .color(style::color::TEXT_DIM),
                    );
                    ui.add_space(8.0);
                    if ui.button("Update selected").clicked() && !busy {
                        let names: Vec<String> = self.selected.iter().cloned().collect();
                        self.run_op("Updating selected", move || {
                            let mut all = Vec::new();
                            for n in &names {
                                if let Ok(reports) = install::update(Some(n), false) {
                                    all.extend(reports);
                                }
                            }
                            OpDone {
                                message: fmt_reports(all),
                                refresh_apps: true,
                            }
                        });
                        self.selected.clear();
                    }
                    if ui.button("Remove selected").clicked() && !busy {
                        let names: Vec<String> = self.selected.iter().cloned().collect();
                        self.run_op("Removing selected", move || {
                            let mut msgs = Vec::new();
                            for n in &names {
                                match install::remove(n, false) {
                                    Ok(m) => msgs.push(m),
                                    Err(e) => msgs.push(format!("Couldn't remove {n}: {e}")),
                                }
                            }
                            OpDone {
                                message: msgs.join("\n"),
                                refresh_apps: true,
                            }
                        });
                        self.selected.clear();
                    }
                    if style::ghost_button(ui, "Clear").clicked() {
                        self.selected.clear();
                    }
                });
            }
            ui.add_space(4.0);
        });

        // ---- status bar ----
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if let Some(op) = &self.busy {
                    ui.spinner();
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!("{}…", op.label)).color(style::color::TEXT_DIM),
                    );
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
                    ui.add_space(80.0);
                    ui.label(
                        egui::RichText::new("No apps installed yet")
                            .size(22.0)
                            .strong()
                            .color(style::color::TEXT),
                    );
                    ui.add_space(12.0);
                    style::dim_label(ui, "Install your first app to get started.");
                    ui.add_space(24.0);
                    if style::primary_button(ui, "Install app…").clicked() {
                        if let Some(paths) = rfd::FileDialog::new()
                            .set_title("Choose files to install")
                            .pick_files()
                        {
                            for path in paths {
                                self.spawn_installer(&path);
                            }
                        }
                    }
                    ui.add_space(16.0);
                    style::faint_label(
                        ui,
                        "…or double-click any installer file in your file manager.",
                    );
                    ui.add_space(8.0);
                    style::faint_label(ui, "You can also drag and drop files onto this window.");
                });
                return;
            }
            // Filter by search and format, then sort.
            let query = self.search.trim().to_lowercase();
            let mut filtered: Vec<AppRow> = self
                .apps
                .iter()
                .filter(|a| {
                    let matches_query = query.is_empty()
                        || a.name.to_lowercase().contains(&query)
                        || a.format.to_lowercase().contains(&query)
                        || a.category.to_lowercase().contains(&query);
                    let matches_format = self
                        .format_filter
                        .as_ref()
                        .map(|f| &a.format == f)
                        .unwrap_or(true);
                    let matches_category = self
                        .category_filter
                        .as_ref()
                        .map(|c| &a.category == c)
                        .unwrap_or(true);
                    matches_query && matches_format && matches_category
                })
                .cloned()
                .collect();
            // Parse "1.2 GB" etc back to bytes for sorting; unknown last.
            fn parse_size(s: &Option<String>) -> u64 {
                let s = match s {
                    Some(s) => s,
                    None => return 0,
                };
                let (num, unit) = s.split_at(s.find(' ').unwrap_or(s.len()));
                let num: f64 = num.trim().parse().unwrap_or(0.0);
                let mult = match unit.trim() {
                    "GB" => 1024u64.pow(3) as f64,
                    "MB" => 1024u64.pow(2) as f64,
                    "KB" => 1024.0,
                    _ => 1.0,
                };
                (num * mult) as u64
            }
            match self.sort_by {
                SortBy::Name => {
                    filtered.sort_by_key(|a| a.name.to_lowercase());
                }
                SortBy::Recent => filtered.sort_by(|a, b| b.installed_on.cmp(&a.installed_on)),
                SortBy::Size => {
                    filtered.sort_by_key(|a| std::cmp::Reverse(parse_size(&a.size)));
                }
            }
            if filtered.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.label(
                        egui::RichText::new("No apps match your search")
                            .size(20.0)
                            .strong()
                            .color(style::color::TEXT),
                    );
                    ui.add_space(8.0);
                    style::dim_label(ui, "Try a different search term.");
                });
                return;
            }
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} app{} installed",
                        self.apps.len(),
                        if self.apps.len() == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(style::color::TEXT_FAINT),
                );
                // Filters on the right. Reserve width so they never
                // overlap the count on narrow windows.
                let right_width = 420.0;
                let avail = ui.available_width();
                if avail > right_width {
                    ui.add_space(avail - right_width);
                }
                ui.horizontal(|ui| {
                    // Select mode toggle.
                    if ui.checkbox(&mut self.select_mode, "Select").changed() && !self.select_mode {
                        self.selected.clear();
                    }
                    ui.add_space(8.0);
                    // Sort.
                    let sort_label = self.sort_by.label().to_string();
                    style::filter_dropdown(ui, &sort_label, 120.0, |ui| {
                        for opt in [SortBy::Name, SortBy::Size, SortBy::Recent] {
                            ui.selectable_value(&mut self.sort_by, opt, opt.label());
                        }
                    });
                    ui.add_space(8.0);
                    // Format filter.
                    let mut formats: Vec<String> = self
                        .apps
                        .iter()
                        .map(|a| a.format.clone())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    formats.sort();
                    let format_label = self
                        .format_filter
                        .as_deref()
                        .unwrap_or("All formats")
                        .to_string();
                    let formats_clone = formats.clone();
                    style::filter_dropdown(ui, &format_label, 120.0, |ui| {
                        if ui
                            .selectable_value(&mut self.format_filter, None, "All formats")
                            .clicked()
                        {
                            self.selected.clear();
                        }
                        for f in &formats_clone {
                            if ui
                                .selectable_value(&mut self.format_filter, Some(f.clone()), f)
                                .clicked()
                            {
                                self.selected.clear();
                            }
                        }
                    });
                    // Category filter (only when useful).
                    let mut categories: Vec<String> = self
                        .apps
                        .iter()
                        .map(|a| a.category.clone())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    categories.sort();
                    if categories.len() > 1 {
                        ui.add_space(8.0);
                        let cat_label = self
                            .category_filter
                            .as_deref()
                            .unwrap_or("All categories")
                            .to_string();
                        let cats_clone = categories.clone();
                        style::filter_dropdown(ui, &cat_label, 120.0, |ui| {
                            if ui
                                .selectable_value(&mut self.category_filter, None, "All categories")
                                .clicked()
                            {
                                self.selected.clear();
                            }
                            for c in &cats_clone {
                                if ui
                                    .selectable_value(&mut self.category_filter, Some(c.clone()), c)
                                    .clicked()
                                {
                                    self.selected.clear();
                                }
                            }
                        });
                    }
                });
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                for app in &filtered {
                    // Clone the row: app_card may mutate self (dialogs, ops).
                    let app = app.clone();
                    if self.select_mode {
                        let mut checked = self.selected.contains(&app.name);
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut checked, "").changed() {
                                if checked {
                                    self.selected.insert(app.name.clone());
                                } else {
                                    self.selected.remove(&app.name);
                                }
                            }
                            self.app_card(ui, &app, busy);
                        });
                    } else {
                        self.app_card(ui, &app, busy);
                    }
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
            Launch,
            EnvVars,
            Details,
            SwitchRunner(bool),
        }
        let mut act = Act::None;

        style::card_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                style::app_icon(ui, &app.name, &app.name, 52.0);
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    // App name: prominent, with verification check.
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&app.name)
                                .size(18.0)
                                .strong()
                                .color(style::color::TEXT),
                        );
                        if app.verified {
                            ui.label(
                                egui::RichText::new("✓")
                                    .size(16.0)
                                    .color(style::color::GOOD)
                                    .strong(),
                            );
                        }
                    });
                    ui.add_space(6.0);
                    // Badges: format, version, status.
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        style::badge(ui, &app.format, style::format_color(&app.format));
                        if let Some(v) = &app.version {
                            style::badge(ui, &format!("v{v}"), style::color::TEXT_DIM);
                        }
                        if app.has_env {
                            style::badge(ui, "env", style::color::INFO);
                        }
                        if app.has_update {
                            style::badge(ui, "update available", style::color::WARN);
                        }
                        if app.recently_updated {
                            style::badge(ui, "updated", style::color::GOOD);
                        }
                    });
                    // Metadata: subtle, secondary.
                    let mut meta = Vec::new();
                    if let Some(repo) = &app.github_repo {
                        meta.push(format!("tracking {repo}"));
                    }
                    if let Some(s) = &app.size {
                        meta.push(s.clone());
                    }
                    if let Some(d) = &app.installed_on {
                        meta.push(format!("installed {d}"));
                    }
                    if !meta.is_empty() {
                        ui.add_space(6.0);
                        style::faint_label(ui, &meta.join("  ·  "));
                    }
                });
                // Primary action: Launch. Everything else in the … menu.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let is_script = app.format == "installer script";
                    style::tools_menu_button(ui, |ui| {
                        ui.set_min_width(160.0);
                        if !is_script && ui.button("Check for update").clicked() && !busy {
                            act = Act::Update;
                            ui.close();
                        }
                        if app.has_snapshots && ui.button("Rollback…").clicked() && !busy {
                            act = Act::Rollback;
                            ui.close();
                        }
                        if ui.button("Environment…").clicked() && !busy {
                            act = Act::EnvVars;
                            ui.close();
                        }
                        if ui.button("Details…").clicked() {
                            act = Act::Details;
                            ui.close();
                        }
                        // Runner switch for Windows apps.
                        if app.format.contains("Windows") {
                            let current = dropin_core::handlers::wine::current_runner(
                                &dropin_core::slugify(&app.name),
                            );
                            let is_proton =
                                current == Some(dropin_core::handlers::proton::Runner::Proton);
                            let label = if is_proton {
                                "Switch to Wine…"
                            } else {
                                "Switch to GE-Proton…"
                            };
                            if ui.button(label).clicked() && !busy {
                                act = Act::SwitchRunner(!is_proton);
                                ui.close();
                            }
                        }
                        if !is_script {
                            if app.github_repo.is_some() {
                                if ui.button("Stop tracking").clicked() && !busy {
                                    act = Act::Untrack;
                                    ui.close();
                                }
                            } else if ui.button("Track GitHub releases…").clicked() && !busy {
                                act = Act::Track;
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui
                            .button(egui::RichText::new("Remove…").color(style::color::BAD))
                            .clicked()
                            && !busy
                        {
                            act = Act::Remove;
                            ui.close();
                        }
                    });
                    ui.add_space(8.0);
                    if style::primary_button_enabled(ui, !busy, "Launch").clicked() && !busy {
                        act = Act::Launch;
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
                OpDone {
                    message: msg,
                    refresh_apps: true,
                }
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
                OpDone {
                    message,
                    refresh_apps: true,
                }
            }),
            Act::Launch => {
                let message =
                    install::launch(&name).unwrap_or_else(|e| format!("Couldn't launch: {e}"));
                self.status = message;
            }
            Act::EnvVars => {
                self.env_name = Some(name);
                self.env_vars = self.load_env_vars();
                self.env_new_key.clear();
                self.env_new_value.clear();
            }
            Act::Details => {
                self.details_name = Some(name);
            }
            Act::SwitchRunner(to_proton) => {
                let runner = if to_proton {
                    dropin_core::handlers::proton::Runner::Proton
                } else {
                    dropin_core::handlers::proton::Runner::Wine
                };
                let slug = dropin_core::slugify(&name);
                self.run_op(&format!("Switching {name}"), move || {
                    let message = dropin_core::handlers::wine::set_runner(&slug, runner)
                        .unwrap_or_else(|e| format!("Couldn't switch runner: {e}"));
                    OpDone {
                        message,
                        refresh_apps: true,
                    }
                });
            }
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
                    ui.label(
                        egui::RichText::new("Download it. Double-click it. Done.")
                            .size(18.0)
                            .strong(),
                    );
                    ui.add_space(10.0);
                    style::dim_label(
                        ui,
                        "dropin installs AppImages, .debs, .rpms, tarballs, Flatpaks, \
                         and installer scripts — all without root, all fully reversible.",
                    );
                    ui.add_space(10.0);
                    style::section_title(ui, "One-click setup");
                    style::dim_label(
                        ui,
                        "• Double-click any installer file to open dropin\n\
                         • “Install as application” in your file manager's right-click menu\n\
                         • App menu entries for everything you install",
                    );
                    ui.add_space(10.0);
                    style::section_title(ui, "Tips");
                    style::dim_label(
                        ui,
                        "• Drag and drop files onto this window to install\n\
                         • Click … on any app for details, env vars, and more\n\
                         • Use Search (Ctrl+F) and filters to find apps quickly",
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if style::ghost_button(ui, "Later").clicked() {
                                close = true;
                            }
                            ui.add_enabled_ui(!busy, |ui| {
                                if style::primary_button(ui, "Set up dropin").clicked() {
                                    setup = true;
                                }
                            });
                        });
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
                    OpDone {
                        message,
                        refresh_apps: false,
                    }
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
                    OpDone {
                        message,
                        refresh_apps: true,
                    }
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
                    ui.checkbox(
                        &mut self.remove_purge,
                        "Also move leftover config/cache/data to the trash",
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if style::ghost_button(ui, "Cancel").clicked() {
                                close = true;
                            }
                            ui.add_enabled_ui(!busy, |ui| {
                                let btn = egui::Button::new(
                                    egui::RichText::new("Remove")
                                        .strong()
                                        .color(style::color::TEXT),
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
                    OpDone {
                        message,
                        refresh_apps: true,
                    }
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
                    OpDone {
                        message,
                        refresh_apps: true,
                    }
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
                    // Launch at startup toggle.
                    let mut autostart = is_autostart_enabled();
                    if ui.checkbox(&mut autostart, "Open dropin when I log in").changed() {
                        set_autostart(autostart);
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
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                for issue in &self.doctor_issues {
                                    let tag = match issue.kind {
                                        health::HealthKind::Orphaned => "orphaned",
                                        health::HealthKind::Duplicate => "duplicate",
                                        health::HealthKind::Shadowed => "shadowed",
                                        health::HealthKind::CrossSource => "also-installed",
                                        health::HealthKind::BrokenExec => "broken-menu",
                                        health::HealthKind::NoFuse => "no-fuse",
                                    };
                                    ui.label(
                                        egui::RichText::new(format!("[{tag}] {}", issue.message))
                                            .strong(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!("hint: {}", issue.hint)).weak(),
                                    );
                                    ui.add_space(6.0);
                                }
                            });
                    }
                    // Wine status for Windows apps.
                    ui.add_space(12.0);
                    style::section_title(ui, "Windows apps");
                    if dropin_core::handlers::wine::have_wine() {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("●").color(style::color::GOOD).strong());
                            style::dim_label(
                                ui,
                                "Wine is installed — .exe files can be installed.",
                            );
                        });
                    } else {
                        style::dim_label(
                            ui,
                            "Wine isn't installed. dropin can set it up automatically.",
                        );
                        ui.add_space(4.0);
                        if style::primary_button_enabled(ui, !busy, "Set up Wine").clicked() {
                            self.run_op("Setting up Wine", || {
                                let message =
                                    match dropin_core::handlers::wine_setup::auto_setup(&|_| {}) {
                                        Ok(_) => "Wine is ready.".to_string(),
                                        Err(e) => format!("Wine setup failed: {e}"),
                                    };
                                OpDone {
                                    message,
                                    refresh_apps: false,
                                }
                            });
                        }
                    }
                });
            if !open {
                self.doctor_open = false;
            }
        }

        // ---- env var editor ----
        if let Some(name) = self.env_name.clone() {
            let mut open = true;
            let mut close = false;
            egui::Window::new(format!("Environment for {name}"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    style::dim_label(
                        ui,
                        "These variables are set when you launch the app from dropin.",
                    );
                    ui.add_space(8.0);
                    let mut remove_idx: Option<usize> = None;
                    for (i, (k, v)) in self.env_vars.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(k);
                            ui.label("=");
                            ui.text_edit_singleline(v);
                            if style::ghost_button(ui, "✕").clicked() {
                                remove_idx = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove_idx {
                        self.env_vars.remove(i);
                        self.save_env_vars();
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.env_new_key);
                        ui.label("=");
                        ui.text_edit_singleline(&mut self.env_new_value);
                        if style::primary_button_enabled(
                            ui,
                            !self.env_new_key.trim().is_empty(),
                            "Add",
                        )
                        .clicked()
                        {
                            self.env_vars.push((
                                self.env_new_key.trim().to_string(),
                                self.env_new_value.clone(),
                            ));
                            self.env_new_key.clear();
                            self.env_new_value.clear();
                            self.save_env_vars();
                        }
                    });
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if style::primary_button(ui, "Done").clicked() {
                                close = true;
                            }
                        });
                    });
                });
            if !open || close {
                self.env_name = None;
            }
        }

        // ---- details view ----
        if let Some(name) = self.details_name.clone() {
            let app = self.apps.iter().find(|a| a.name == name).cloned();
            let mut open = true;
            let mut close = false;
            if let Some(app) = app {
                egui::Window::new(format!("{} — details", app.name))
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            style::app_icon(ui, &app.name, &app.name, 56.0);
                            ui.add_space(8.0);
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&app.name).size(20.0).strong());
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    style::badge(ui, &app.format, style::format_color(&app.format));
                                    if let Some(v) = &app.version {
                                        style::badge(ui, &format!("v{v}"), style::color::BORDER);
                                    }
                                    if app.verified {
                                        style::badge(ui, "✓ verified", style::color::GOOD);
                                    }
                                    if app.has_update {
                                        style::badge(ui, "update available", style::color::WARN);
                                    }
                                });
                            });
                        });
                        ui.add_space(12.0);
                        egui::Grid::new("details_grid")
                            .num_columns(2)
                            .spacing([12.0, 6.0])
                            .show(ui, |ui| {
                                if let Some(s) = &app.size {
                                    ui.label(
                                        egui::RichText::new("Size").color(style::color::TEXT_DIM),
                                    );
                                    ui.label(s);
                                    ui.end_row();
                                }
                                if let Some(d) = &app.installed_on {
                                    ui.label(
                                        egui::RichText::new("Installed")
                                            .color(style::color::TEXT_DIM),
                                    );
                                    ui.label(d);
                                    ui.end_row();
                                }
                                if let Some(p) = &app.install_root {
                                    ui.label(
                                        egui::RichText::new("Location")
                                            .color(style::color::TEXT_DIM),
                                    );
                                    ui.label(egui::RichText::new(p).small().monospace());
                                    ui.end_row();
                                }
                                if let Some(s) = &app.source_path {
                                    ui.label(
                                        egui::RichText::new("Source").color(style::color::TEXT_DIM),
                                    );
                                    ui.label(egui::RichText::new(s).small().monospace());
                                    ui.end_row();
                                }
                                if let Some(r) = &app.github_repo {
                                    ui.label(
                                        egui::RichText::new("Tracking")
                                            .color(style::color::TEXT_DIM),
                                    );
                                    ui.label(r);
                                    ui.end_row();
                                }
                                ui.label(
                                    egui::RichText::new("Category").color(style::color::TEXT_DIM),
                                );
                                ui.label(&app.category);
                                ui.end_row();
                            });
                        if !app.env_vars.is_empty() {
                            ui.add_space(8.0);
                            style::section_title(ui, "Environment");
                            for (k, v) in &app.env_vars {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(k).strong().monospace());
                                    ui.label("=");
                                    ui.label(egui::RichText::new(v).monospace());
                                });
                            }
                        }
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if style::primary_button(ui, "Close").clicked() {
                                        close = true;
                                    }
                                    // Show in file manager button.
                                    if let Some(root) = &app.install_root {
                                        if style::ghost_button(ui, "Show files").clicked() {
                                            let _ = std::process::Command::new("xdg-open")
                                                .arg(root)
                                                .spawn();
                                        }
                                    }
                                },
                            );
                        });
                    });
            }
            if !open || close {
                self.details_name = None;
            }
        }
    }
}
