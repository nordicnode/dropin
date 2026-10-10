//! dropin-gui: the graphical interface.
//!
//! With a file argument: the double-click install dialog (registered as the
//! MIME handler for AppImage/.deb/.rpm/.flatpakref/installer scripts).
//! Without one: the manager window — installed apps, updates, rollback,
//! removal, GitHub tracking, and doctor. Everything the CLI does, no
//! terminal needed.

mod manager;
mod style;

use dropin_core::detect::{self, Detection};
use dropin_core::handlers::{script, InstallOptions, InstallPlan};
use dropin_core::install;
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Hidden testing aid: skip the review screen and install immediately.
    let auto_install = args.iter().any(|a| a == "--auto-install");
    let file = args
        .iter()
        .find(|a| *a != "--auto-install" && *a != "--manager")
        .map(PathBuf::from);

    // No file: open the manager. `--manager` forces it even with a file arg
    // present (used by .desktop launchers).
    let want_manager = file.is_none() || args.iter().any(|a| a == "--manager");
    if want_manager {
        let native = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([720.0, 540.0]),
            ..Default::default()
        };
        if let Err(e) = eframe::run_native(
            "dropin",
            native,
            Box::new(|cc| {
                style::apply_theme(&cc.egui_ctx);
                Ok(Box::new(manager::ManagerApp::new()))
            }),
        ) {
            eprintln!("dropin-gui: {e}");
            std::process::exit(1);
        }
        return;
    }

    let file = file.unwrap();
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 640.0])
            .with_resizable(false),
        ..Default::default()
    };
    let mut app = DropinApp::new(file);
    if auto_install && matches!(app.phase, Phase::Review(_)) {
        app.start_install();
    }
    let title = "Install with dropin";
    if let Err(e) = eframe::run_native(
        title,
        native,
        Box::new(|cc| {
            style::apply_theme(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    ) {
        eprintln!("dropin-gui: {e}");
        std::process::exit(1);
    }
}

struct InstallResult {
    ok: bool,
    message: String,
    app_name: String,
    /// Plain-language report for script installs (None for other formats).
    script_report: Option<script::ScriptReport>,
}

struct ReviewState {
    detection: Detection,
    plan: InstallPlan,
    size: u64,
    sha: String,
    verified: dropin_core::verify::Verification,
    trust_checked: bool,
    /// Script-installer safety audit (empty for other formats).
    findings: Vec<script::Finding>,
    allow_net: bool,
}

enum Phase {
    /// The file couldn't even be identified.
    Fatal(String),
    Review(ReviewState),
    Installing { rx: Receiver<InstallResult> },
    Done(InstallResult),
}

struct DropinApp {
    file: PathBuf,
    phase: Phase,
    /// Remembered across a retry (script installs).
    allow_net: bool,
}

impl DropinApp {
    fn new(file: PathBuf) -> Self {
        let phase = match detect::detect(&file) {
            Err(e) => Phase::Fatal(format!("dropin can't handle this file:\n{e}")),
            Ok(detection) => match install::plan_for(&file) {
                Err(e) => Phase::Fatal(format!("{e}")),
                Ok(plan) => {
                    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                    let full_sha = dropin_core::handlers::sha256_of(&file).ok();
                    let verified = match &full_sha {
                        Some(sha) => dropin_core::verify::verify(&file, sha),
                        None => dropin_core::verify::Verification::Unverified,
                    };
                    let sha = full_sha
                        .map(|s| {
                            if s.len() > 16 {
                                format!("{}…", &s[..16])
                            } else {
                                s
                            }
                        })
                        .unwrap_or_else(|| "unavailable".into());
                    // Script installers get a safety audit up front.
                    let findings = if detection.format == detect::Format::Script {
                        script::audit(&file)
                    } else {
                        Vec::new()
                    };
                    Phase::Review(ReviewState {
                        detection,
                        plan,
                        size,
                        sha,
                        verified,
                        trust_checked: false,
                        findings,
                        allow_net: false,
                    })
                }
            },
        };
        Self { file, phase, allow_net: false }
    }

    fn start_install(&mut self) {
        let file = self.file.clone();
        let allow_net = self.allow_net;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let res = install::install(
                &file,
                &InstallOptions {
                    yes: true, // the GUI already got explicit confirmation
                    keep_source: false,
                    system: false,
                    allow_net,
                    quiet: true,
                },
            );
            let _ = tx.send(match res {
                Ok(o) => {
                    // Script installs write a plain-language report; pick it up.
                    let script_report = script::read_report(&o.name);
                    InstallResult {
                        ok: true,
                        message: format!(
                            "Installed to your user directories{}\n\nManage it anytime in the dropin manager.",
                            if o.external { " (tracked as external)" } else { "" },
                        ),
                        app_name: o.name,
                        script_report,
                    }
                }
                Err(e) => {
                    // Even failed script installs leave a report with the
                    // plain-language failure hint. Recover the slug the same
                    // way the installer computes it (detection + slugify).
                    let slug = detect::detect(&file)
                        .map(|d| dropin_core::slugify(&d.name_hint))
                        .unwrap_or_else(|_| "app".into());
                    InstallResult {
                        ok: false,
                        message: format!("Install failed:\n{e}"),
                        app_name: String::new(),
                        script_report: script::read_report(&slug),
                    }
                }
            });
        });
        self.phase = Phase::Installing { rx };
    }
}



impl eframe::App for DropinApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll the install thread.
        if let Phase::Installing { rx } = &self.phase {
            if let Ok(res) = rx.try_recv() {
                self.phase = Phase::Done(res);
            } else {
                ctx.request_repaint();
            }
        }

        // Actions decided during rendering, applied after the borrow ends.
        enum Next {
            None,
            StartInstall,
            RetryWithNet,
            Exit(i32),
        }
        let mut next = Next::None;
        egui::CentralPanel::default().show(ctx, |ui| {
            if std::env::var("DROPIN_GUI_DEBUG").is_ok() {
                eprintln!(
                    "DEBUG screen_rect={:?} avail={:?} ppp={}",
                    ctx.screen_rect(),
                    ui.available_rect_before_wrap(),
                    ctx.pixels_per_point()
                );
                std::env::remove_var("DROPIN_GUI_DEBUG");
            }
            ui.add_space(12.0);
            match &mut self.phase {
                Phase::Fatal(msg) => {
                    let msg = msg.clone();
                    ui.heading("Can't install this file");
                    ui.add_space(8.0);
                    ui.label(msg);
                    ui.add_space(16.0);
                    if ui.button("Close").clicked() {
                        next = Next::Exit(1);
                    }
                }
                Phase::Review(st) => {
                    if ui_review(ui, &self.file, st) {
                        self.allow_net = st.allow_net;
                        next = Next::StartInstall;
                    }
                }
                Phase::Installing { .. } => ui_installing(ui),
                Phase::Done(res) => match ui_done(ui, res) {
                    DoneAction::Close => {
                        next = Next::Exit(if res.ok { 0 } else { 1 });
                    }
                    DoneAction::RetryWithNet => {
                        next = Next::RetryWithNet;
                    }
                    DoneAction::None => {}
                },
            }
        });
        match next {
            Next::None => {}
            Next::StartInstall => self.start_install(),
            Next::RetryWithNet => {
                self.allow_net = true;
                self.start_install();
            }
            Next::Exit(code) => std::process::exit(code),
        }
    }
}

/// Returns true when the user clicked Install.
fn ui_review(ui: &mut egui::Ui, file: &std::path::Path, st: &mut ReviewState) -> bool {
        // Hero: app badge, name, version, format pill.
        ui.horizontal(|ui| {
            style::app_badge(ui, &st.plan.app_name, 64.0);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(&st.plan.app_name)
                        .size(24.0)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    style::badge(
                        ui,
                        st.detection.format.label(),
                        style::format_color(st.detection.format.label()),
                    );
                    if let Some(v) = &st.plan.version {
                        style::badge(ui, &format!("v{v}"), style::color::BORDER);
                    }
                    style::badge(ui, &style::human_size(st.size), style::color::BORDER);
                });
            });
        });

        ui.add_space(4.0);
        style::faint_label(ui, &file.display().to_string());
        ui.label(egui::RichText::new(file.display().to_string()).weak().small());
        style::faint_label(ui, &format!("sha256 {}", st.sha));
        ui.add_space(2.0);
        match &st.verified {
            dropin_core::verify::Verification::Verified { .. } => {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("●")
                            .color(style::color::GOOD)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new("Checksum verified")
                            .color(style::color::GOOD),
                    );
                });
            }
            dropin_core::verify::Verification::Mismatch { .. } => {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("●")
                            .color(style::color::BAD)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new("Checksum mismatch — install will be refused")
                            .color(style::color::BAD)
                            .strong(),
                    );
                });
            }
            dropin_core::verify::Verification::Unverified => {
                style::dim_label(ui, "No checksum file found — skipping verification");
            }
        }

        ui.add_space(14.0);
        style::section_title(ui, "What will happen");
        for step in &st.plan.steps {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("→").color(style::color::TEXT_FAINT));
                style::dim_label(ui, step);
            });
        }
        if st.plan.needs_root {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Requires root access.")
                    .color(style::color::WARN)
                    .strong(),
            );
        }

        let needs_trust = st.plan.needs_confirmation;
        if needs_trust {
            ui.add_space(14.0);
            // Safety audit: plain-language findings, most severe first.
            style::section_title(ui, "Safety check");
            egui::ScrollArea::vertical()
                .max_height(170.0)
                .show(ui, |ui| {
                    for f in &st.findings {
                        let color = match f.severity {
                            script::Severity::Info => style::color::GOOD,
                            script::Severity::Warning => style::color::WARN,
                            script::Severity::Danger => style::color::BAD,
                        };
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("●").color(color).strong());
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&f.title).strong());
                                style::dim_label(ui, &f.detail);
                            });
                        });
                        ui.add_space(8.0);
                    }
                });
            // Internet toggle, only when the audit says it's needed.
            if st.findings.iter().any(|f| f.needs_net) {
                ui.add_space(8.0);
                ui.checkbox(&mut st.allow_net, "Allow internet access just for this install");
                style::faint_label(
                    ui,
                    "This installer downloads extra files. Dropin blocks the internet \
                     by default; allowing it applies to this install only.",
                );
            }
            ui.add_space(10.0);
            style::card_frame()
                .show(ui, |ui| {
                    style::dim_label(
                        ui,
                        "The installer runs sandboxed: no admin rights, only your install \
                         folders are writable, and everything it does is logged. Removing \
                         the app later undoes it completely.",
                    );
                    ui.add_space(6.0);
                    ui.checkbox(
                        &mut st.trust_checked,
                        "I trust this installer and want to run it",
                    );
                });
        }

        ui.add_space(20.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let checksum_ok = !matches!(
                st.verified,
                dropin_core::verify::Verification::Mismatch { .. }
            );
            let can_install = (!needs_trust || st.trust_checked) && checksum_ok;
            let clicked = style::primary_button_enabled(ui, can_install, "Install").clicked();
            if style::ghost_button(ui, "Cancel").clicked() {
                std::process::exit(0);
            }
            clicked
        })
        .inner
    }

fn ui_installing(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(80.0);
        ui.spinner();
        ui.add_space(16.0);
        ui.label(egui::RichText::new("Installing…").size(20.0).strong());
        ui.add_space(8.0);
        style::dim_label(ui, "This usually takes a few seconds.");
    });
}

enum DoneAction {
    None,
    Close,
    RetryWithNet,
}

/// Done screen. For script installs, shows the plain-language report,
/// a log viewer, and a one-click retry with internet when that was the
/// likely blocker.
fn ui_done(ui: &mut egui::Ui, res: &InstallResult) -> DoneAction {
    let mut action = DoneAction::None;
    ui.add_space(16.0);
    if let Some(rep) = &res.script_report {
        // Plain-language report.
        if rep.success {
            ui.label(
                egui::RichText::new(format!("✓ Installed “{}”", res.app_name))
                    .size(22.0)
                    .strong()
                    .color(style::color::GOOD),
            );
        } else {
            ui.label(
                egui::RichText::new("✗ The installer didn't finish")
                    .size(22.0)
                    .strong()
                    .color(style::color::BAD),
            );
        }
        ui.add_space(10.0);
        style::dim_label(ui, &script::describe_report(&res.app_name, rep));
        if rep.success {
            ui.add_space(8.0);
            style::faint_label(
                ui,
                &format!(
                    "{} added, {} removed, {} changed — removing “{}” undoes it all.",
                    rep.added.len(),
                    rep.removed.len(),
                    rep.modified.len(),
                    res.app_name,
                ),
            );
        }
        // Retry with internet when the failure smells like a blocked network.
        let net_hint = !rep.net_allowed
            && rep
                .failure_hint
                .as_deref()
                .map(|h| h.contains("internet"))
                .unwrap_or(false);
        if !rep.success && net_hint {
            ui.add_space(12.0);
            if style::primary_button(ui, "Try again with internet allowed").clicked() {
                action = DoneAction::RetryWithNet;
            }
        }
        // Log viewer.
        ui.add_space(10.0);
        let log_text =
            std::fs::read_to_string(&rep.log_path).unwrap_or_else(|_| "(log unavailable)".into());
        let tail: String = log_text.lines().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        egui::CollapsingHeader::new("View install log")
            .default_open(false)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(tail).monospace().small(),
                        );
                    });
            });
    } else if res.ok {
        ui.label(
            egui::RichText::new(format!("✓ Installed “{}”", res.app_name))
                .size(22.0)
                .strong()
                .color(style::color::GOOD),
        );
        ui.add_space(10.0);
        style::dim_label(ui, &res.message);
    } else {
        ui.label(
            egui::RichText::new("✗ Something went wrong")
                .size(22.0)
                .strong()
                .color(style::color::BAD),
        );
        ui.add_space(10.0);
        style::dim_label(ui, &res.message);
    }
    ui.add_space(20.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if style::primary_button(ui, "Close").clicked() {
            action = DoneAction::Close;
        }
    });
    action
}
