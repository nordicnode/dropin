//! dropin CLI: install anything, track everything.

use clap::{Parser, Subcommand};
use dropin_core::handlers::InstallOptions;
use dropin_core::{detect, install};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dropin",
    version,
    about = "Download it. Double-click it. Done."
)]
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
        /// Allow installer scripts (.sh/.run/.bin) internet access in the sandbox.
        #[arg(long)]
        allow_net: bool,
    },
    /// Remove an app installed by dropin.
    Remove {
        name: String,
        /// Also move leftover config/cache/data dirs to the trash.
        #[arg(long)]
        purge: bool,
    },
    /// Check installed apps for updates (the GUI manager can also auto-check on launch).
    Update {
        /// Update a single app by name.
        name: Option<String>,
        /// Check everything dropin installed.
        #[arg(long)]
        all: bool,
    },
    /// Restore an AppImage to its previous installed version.
    Rollback {
        /// Name of the app to roll back.
        name: String,
    },
    /// Track a GitHub repo's releases for an installed AppImage.
    Track {
        /// Name of the installed app.
        name: String,
        /// GitHub repo as owner/repo.
        #[arg(long)]
        github: String,
    },
    /// Stop tracking a GitHub repo for an app.
    Untrack {
        /// Name of the installed app.
        name: String,
    },
    /// List apps installed by dropin.
    List,
    /// Check helper tools and environment.
    Doctor,
    /// Set up Wine automatically for Windows apps (tries system package
    /// manager first, falls back to portable Wine).
    SetupWine,
    /// Set up Proton (GE-Proton via umu-launcher) automatically for games.
    SetupProton,
    /// Register dropin with the desktop: double-click handling plus
    /// “Install as application” in Dolphin, Nemo, Nautilus, and Thunar.
    Integrate,
    /// Remove all desktop and file-manager integration.
    Unintegrate,
    /// Launch an installed app.
    Launch {
        /// Name of the app to launch.
        name: String,
    },
    /// Set an environment variable for an app (used when launching).
    SetEnv {
        /// Name of the app.
        name: String,
        /// Variable name.
        key: String,
        /// Variable value.
        value: String,
    },
    /// Remove an environment variable from an app.
    UnsetEnv {
        /// Name of the app.
        name: String,
        /// Variable name.
        key: String,
    },
    /// Export the app list to a JSON file (for backup or migration).
    Export {
        /// Output file path.
        file: PathBuf,
    },
    /// Import an app list from a JSON file.
    Import {
        /// Input file path.
        file: PathBuf,
    },
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
                if d.by_magic {
                    "(magic bytes)"
                } else {
                    "(extension)"
                }
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
            allow_net,
        } => {
            let opts = InstallOptions {
                yes,
                keep_source,
                system,
                allow_net,
                quiet: false,
                runner: None,
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
        Cmd::Remove { name, purge } => {
            let msg = install::remove(&name, purge)?;
            println!("{msg}");
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
        Cmd::Rollback { name } => {
            let msg = install::rollback(&name)?;
            println!("{msg}.");
            Ok(())
        }
        Cmd::Track { name, github } => {
            let msg = install::track(&name, &github)?;
            println!("{msg}.");
            Ok(())
        }
        Cmd::Untrack { name } => {
            let msg = install::untrack(&name)?;
            println!("{msg}.");
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
            if !dropin_core::handlers::wine::have_wine() {
                println!("\nWine is missing. Run `dropin setup-wine` to install it automatically.");
            }
            let issues = dropin_core::health::check();
            if issues.is_empty() {
                println!("\nhealth: no issues found");
            } else {
                println!("\nhealth: {} issue(s) found", issues.len());
                for i in issues {
                    let tag = match i.kind {
                        dropin_core::health::HealthKind::Orphaned => "orphaned",
                        dropin_core::health::HealthKind::Duplicate => "duplicate",
                        dropin_core::health::HealthKind::Shadowed => "shadowed",
                        dropin_core::health::HealthKind::CrossSource => "also-installed",
                        dropin_core::health::HealthKind::BrokenExec => "broken-menu",
                        dropin_core::health::HealthKind::NoFuse => "no-fuse",
                    };
                    println!("\n[{tag}] {}", i.message);
                    println!("        hint: {}", i.hint);
                }
            }
            Ok(())
        }
        Cmd::SetupWine => {
            println!("Setting up Wine for Windows apps…");
            match dropin_core::handlers::wine_setup::auto_setup(&|msg| println!("  {msg}")) {
                Ok(path) => {
                    println!("\nWine is ready: {}", path.display());
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Cmd::SetupProton => {
            println!("Setting up Proton for games…");
            match dropin_core::handlers::proton::setup_proton(&|msg| println!("  {msg}")) {
                Ok(path) => {
                    println!("\nProton support is ready: {}", path.display());
                    println!("GE-Proton will download automatically on first game launch.");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Cmd::Integrate => dropin_core::integrate::integrate(true),
        Cmd::Unintegrate => dropin_core::integrate::integrate(false),
        Cmd::Launch { name } => {
            let msg = install::launch(&name)?;
            println!("{msg}");
            Ok(())
        }
        Cmd::SetEnv { name, key, value } => {
            let ledger = dropin_core::ledger::Ledger::open()?;
            let mut env = ledger.env_vars(&name).unwrap_or_default();
            env.insert(key.clone(), value.clone());
            let json = serde_json::to_string(&env).map_err(|e| {
                dropin_core::Error::Unsupported(format!("couldn't save env vars: {e}"))
            })?;
            ledger.set_env_vars(&dropin_core::slugify(&name), &json)?;
            println!("Set {key} for '{name}'.");
            Ok(())
        }
        Cmd::UnsetEnv { name, key } => {
            let ledger = dropin_core::ledger::Ledger::open()?;
            let mut env = ledger.env_vars(&name).unwrap_or_default();
            env.remove(&key);
            let json = serde_json::to_string(&env).map_err(|e| {
                dropin_core::Error::Unsupported(format!("couldn't save env vars: {e}"))
            })?;
            ledger.set_env_vars(&dropin_core::slugify(&name), &json)?;
            println!("Removed {key} from '{name}'.");
            Ok(())
        }
        Cmd::Export { file } => {
            let apps = install::list()?;
            let json = serde_json::to_string_pretty(&apps)
                .map_err(|e| dropin_core::Error::Unsupported(format!("couldn't export: {e}")))?;
            std::fs::write(&file, json)?;
            println!("Exported {} apps to {}.", apps.len(), file.display());
            Ok(())
        }
        Cmd::Import { file } => {
            let json = std::fs::read_to_string(&file)?;
            let apps: Vec<dropin_core::ledger::Install> = serde_json::from_str(&json)
                .map_err(|e| dropin_core::Error::Unsupported(format!("couldn't import: {e}")))?;
            println!("Found {} apps in {}.", apps.len(), file.display());
            let mut restored = 0;
            let mut skipped = Vec::new();
            for app in &apps {
                let src = std::path::Path::new(&app.source_path);
                if src.exists() {
                    println!("  Restoring {} from {}…", app.name, src.display());
                    let opts = InstallOptions {
                        yes: true,
                        keep_source: false,
                        system: false,
                        allow_net: false,
                        quiet: false,
                        runner: None,
                    };
                    match install::install(src, &opts) {
                        Ok(_) => {
                            // Restore env vars if they were exported.
                            if let Some(env_json) = &app.env_vars {
                                if let Ok(map) = serde_json::from_str::<
                                    std::collections::BTreeMap<String, String>,
                                >(env_json)
                                {
                                    if let Ok(ledger) = dropin_core::ledger::Ledger::open() {
                                        let _ = ledger.set_env_vars(
                                            &dropin_core::slugify(&app.name),
                                            &serde_json::to_string(&map).unwrap_or_default(),
                                        );
                                    }
                                }
                            }
                            restored += 1;
                        }
                        Err(e) => skipped.push(format!("{}: {e}", app.name)),
                    }
                } else {
                    skipped.push(format!(
                        "{}: source file not found ({})",
                        app.name, app.source_path
                    ));
                }
            }
            println!("\nRestored {restored} apps.");
            if !skipped.is_empty() {
                println!("Skipped:");
                for s in &skipped {
                    println!("  {s}");
                }
            }
            Ok(())
        }
    }
}
