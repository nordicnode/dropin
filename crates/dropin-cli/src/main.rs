//! dropin CLI: install anything, track everything.

use clap::{Parser, Subcommand};
use dropin_core::handlers::InstallOptions;
use dropin_core::{detect, install};
use std::path::PathBuf;

mod integrate;

#[derive(Parser)]
#[command(name = "dropin", version, about = "Download it. Double-click it. Done.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what installing a file would do (dry run).
    Info { file: PathBuf },
    /// Install a downloaded app file.
    Install {
        file: PathBuf,
        /// Skip confirmations (scripts still ask unless run non-interactively... they ask anyway).
        #[arg(long)]
        yes: bool,
        /// Keep the original downloaded file (AppImages are moved by default).
        #[arg(long)]
        keep_source: bool,
        /// Prefer a real system install via the distro package manager (needs root).
        #[arg(long)]
        system: bool,
    },
    /// Remove an app installed by dropin.
    Remove { name: String },
    /// Check installed apps for updates (on demand only, never in background).
    Update {
        /// Update a single app by name.
        name: Option<String>,
        /// Check everything dropin installed.
        #[arg(long)]
        all: bool,
    },
    /// List apps installed by dropin.
    List,
    /// Check helper tools and environment.
    Doctor,
    /// Register dropin with the desktop: double-click handling plus
    /// “Install as application” in Dolphin, Nemo, Nautilus, and Thunar.
    Integrate,
    /// Remove all desktop and file-manager integration.
    Unintegrate,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("dropin: error: {e}");
        std::process::exit(1);
    }
}

fn run() -> dropin_core::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Info { file } => {
            let d = detect::detect(&file)?;
            let plan = install::plan_for(&file)?;
            println!("file:    {}", file.display());
            println!(
                "format:  {} {}",
                d.format.label(),
                if d.by_magic { "(magic bytes)" } else { "(extension)" }
            );
            println!("app:     {}", plan.app_name);
            if let Some(v) = plan.version {
                println!("version: {v}");
            }
            // Integrity preview: same check install would run.
            if let Ok(sha) = dropin_core::handlers::sha256_of(&file) {
                match dropin_core::verify::verify(&file, &sha) {
                    dropin_core::verify::Verification::Verified { checksum_file } => {
                        println!("integrity: verified ✓ (SHA-256, from {checksum_file})");
                    }
                    dropin_core::verify::Verification::Mismatch { .. } => {
                        println!("integrity: CHECKSUM MISMATCH ✗ — install would be refused");
                    }
                    dropin_core::verify::Verification::Unverified => {
                        println!("integrity: unverified — no checksum file next to the download");
                    }
                }
            }
            if plan.needs_root {
                println!("needs:   root");
            }
            if plan.needs_confirmation {
                println!("needs:   your explicit confirmation");
            }
            println!("would:");
            for s in plan.steps {
                println!("  - {s}");
            }
            Ok(())
        }
        Cmd::Install {
            file,
            yes,
            keep_source,
            system,
        } => {
            let opts = InstallOptions {
                yes,
                keep_source,
                system,
            };
            // Show the plan first unless --yes.
            if !yes {
                let plan = install::plan_for(&file)?;
                println!("Installing '{}' ({}):", plan.app_name, file.display());
                for s in &plan.steps {
                    println!("  - {s}");
                }
                if plan.needs_confirmation {
                    println!("  (confirmation required below)");
                }
                println!();
            }
            let outcome = install::install(&file, &opts)?;
            println!("Installed '{}'.", outcome.name);
            if outcome.external {
                println!("(tracked as external — removal is delegated to the owning tool)");
            }
            println!("Remove anytime with: dropin remove {}", outcome.name);
            Ok(())
        }
        Cmd::Remove { name } => {
            install::remove(&name)?;
            println!("Removed '{name}'.");
            Ok(())
        }
        Cmd::Update { name, all } => {
            use dropin_core::install::UpdateStatus;
            let reports = install::update(name.as_deref(), all)?;
            for r in reports {
                let (tag, msg) = match r.status {
                    UpdateStatus::UpToDate(m) => ("up-to-date", m),
                    UpdateStatus::Updated(m) => ("updated", m),
                    UpdateStatus::Manual(m) => ("manual", m),
                    UpdateStatus::Failed(m) => ("FAILED", m),
                };
                println!("{:12} {:20} {msg}", format!("[{tag}]"), r.name);
            }
            Ok(())
        }
        Cmd::List => {
            let items = install::list()?;
            if items.is_empty() {
                println!("Nothing installed with dropin yet.");
                return Ok(());
            }
            for i in items {
                let ext = if i.external { " [external]" } else { "" };
                let ver = i.version.map(|v| format!(" {v}")).unwrap_or_default();
                let badge = if i.verified { " ✓" } else { "" };
                println!("{}{} — {}{}{ext}", i.name, ver, i.format, badge, ext = ext);
            }
            Ok(())
        }
        Cmd::Doctor => {
            for (tool, ok, why) in install::doctor() {
                println!("{} {:24} ({why})", if ok { "ok " } else { "MISS" }, tool);
            }
            Ok(())
        }
        Cmd::Integrate => integrate::integrate(true),
        Cmd::Unintegrate => integrate::integrate(false),
    }
}

