//! dropin-gui: the double-click dialog.
//!
//! Registered as the MIME handler for AppImage/.deb/.rpm/.flatpakref/
//! installer scripts. Shows what installing would do, gets confirmation,
//! runs the install off the UI thread, reports the result.

use dropin_core::detect::{self, Detection};
use dropin_core::handlers::{InstallOptions, InstallPlan};
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
        .find(|a| *a != "--auto-install")
        .map(PathBuf::from);
    let Some(file) = file else {
        eprintln!("usage: dropin-gui [--auto-install] <file>");
        std::process::exit(2);
    };
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([500.0, 600.0])
            .with_resizable(false),
        ..Default::default()
    };
    let mut app = DropinApp::new(file);
    if auto_install && matches!(app.phase, Phase::Review(_)) {
        app.start_install();
    }
    let title = "Install with dropin";
    if let Err(e) = eframe::run_native(title, native, Box::new(|_| Ok(Box::new(app)))) {
        eprintln!("dropin-gui: {e}");
        std::process::exit(1);
    }
}

struct InstallResult {
    ok: bool,
    message: String,
    app_name: String,
}

struct ReviewState {
    detection: Detection,
    plan: InstallPlan,
    size: u64,
    sha: String,
    verified: dropin_core::verify::Verification,
    trust_checked: bool,
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
                    Phase::Review(ReviewState {
                        detection,
                        plan,
                        size,
                        sha,
                        verified,
                        trust_checked: false,
                    })
                }
            },
        };
        Self { file, phase }
    }

    fn start_install(&mut self) {
        let file = self.file.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let res = install::install(
                &file,
                &InstallOptions {
                    yes: true, // the GUI already got explicit confirmation
                    keep_source: false,
                    system: false,
                },
            );
            let _ = tx.send(match res {
                Ok(o) => InstallResult {
                    ok: true,
                    message: format!(
                        "Installed to your user directories{}\n\nRemove anytime with:\n    dropin remove {}",
                        if o.external { " (tracked as external)" } else { "" },
                        o.name
                    ),
                    app_name: o.name,
                },
                Err(e) => InstallResult {
                    ok: false,
                    message: format!("Install failed:\n{e}"),
                    app_name: String::new(),
                },
            });
        });
        self.phase = Phase::Installing { rx };
    }
}

fn human_size(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
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
                        next = Next::StartInstall;
                    }
                }
                Phase::Installing { .. } => ui_installing(ui),
                Phase::Done(res) => {
                    if ui_done(ui, res) {
                        next = Next::Exit(if res.ok { 0 } else { 1 });
                    }
                }
            }
        });
        match next {
            Next::None => {}
            Next::StartInstall => self.start_install(),
            Next::Exit(code) => std::process::exit(code),
        }
    }
}

/// Returns true when the user clicked Install.
fn ui_review(ui: &mut egui::Ui, file: &std::path::Path, st: &mut ReviewState) -> bool {
        let initial = st
            .plan
            .app_name
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .to_string();

        ui.horizontal(|ui| {
            // App badge: rounded square with the initial.
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
            ui.painter().rect_filled(
                rect,
                12.0,
                egui::Color32::from_rgb(88, 101, 242),
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                initial,
                egui::FontId::proportional(28.0),
                egui::Color32::WHITE,
            );
            ui.vertical(|ui| {
                ui.heading(&st.plan.app_name);
                if let Some(v) = &st.plan.version {
                    ui.label(egui::RichText::new(format!("version {v}")).weak());
                }
            });
        });

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!(
                "{}  •  {}",
                st.detection.format.label(),
                human_size(st.size)
            ))
            .weak(),
        );
        ui.label(egui::RichText::new(file.display().to_string()).weak().small());
        ui.label(egui::RichText::new(format!("sha256 {}", st.sha)).weak().small());
        match &st.verified {
            dropin_core::verify::Verification::Verified { .. } => {
                ui.label(
                    egui::RichText::new("✓ checksum verified")
                        .color(egui::Color32::from_rgb(120, 220, 130)),
                );
            }
            dropin_core::verify::Verification::Mismatch { .. } => {
                ui.label(
                    egui::RichText::new("✗ checksum MISMATCH — install will be refused")
                        .color(egui::Color32::from_rgb(255, 120, 120))
                        .strong(),
                );
            }
            dropin_core::verify::Verification::Unverified => {
                ui.label(
                    egui::RichText::new("unverified — no checksum file found")
                        .weak(),
                );
            }
        }

        ui.add_space(12.0);
        ui.strong("dropin will:");
        for step in &st.plan.steps {
            ui.label(format!("• {step}"));
        }
        if st.plan.needs_root {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Requires root access.").color(egui::Color32::YELLOW));
        }

        let needs_trust = st.plan.needs_confirmation;
        if needs_trust {
            ui.add_space(12.0);
            egui::Frame::NONE
                .fill(egui::Color32::from_rgb(64, 32, 32))
                .corner_radius(8.0)
                .inner_margin(10.0)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "This is a vendor installer script. It will run with your user \
                             privileges and can do anything you can do. Only continue if you \
                             trust where this file came from.",
                        )
                        .color(egui::Color32::from_rgb(255, 180, 180)),
                    );
                    ui.checkbox(
                        &mut st.trust_checked,
                        "I trust this installer and want to run it",
                    );
                });
        }

        ui.add_space(16.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let checksum_ok = !matches!(
                st.verified,
                dropin_core::verify::Verification::Mismatch { .. }
            );
            let can_install = (!needs_trust || st.trust_checked) && checksum_ok;
            let install_btn = ui.add_enabled(
                can_install,
                egui::Button::new(egui::RichText::new("Install").strong()),
            );
            let clicked = install_btn.clicked();
            if ui.button("Cancel").clicked() {
                std::process::exit(0);
            }
            clicked
        })
        .inner
    }

fn ui_installing(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(60.0);
        ui.spinner();
        ui.add_space(12.0);
        ui.heading("Installing…");
        ui.add_space(8.0);
        ui.label(egui::RichText::new("This usually takes a few seconds.").weak());
    });
}

/// Returns true when the user clicked Close.
fn ui_done(ui: &mut egui::Ui, res: &InstallResult) -> bool {
    ui.add_space(20.0);
    if res.ok {
        ui.heading(
            egui::RichText::new(format!("Installed “{}”", res.app_name))
                .color(egui::Color32::from_rgb(120, 220, 130)),
        );
    } else {
        ui.heading(
            egui::RichText::new("Something went wrong")
                .color(egui::Color32::from_rgb(255, 120, 120)),
        );
    }
    ui.add_space(8.0);
    ui.label(&res.message);
    ui.add_space(20.0);
    ui.button("Close").clicked()
}
