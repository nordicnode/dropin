//! dropin CLI: install anything, track everything.

use clap::{Parser, Subcommand};
use dropin_core::handlers::InstallOptions;
use dropin_core::{detect, install};
use std::path::PathBuf;

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
    /// List apps installed by dropin.
    List,
    /// Check helper tools and environment.
    Doctor,
    /// Register dropin-gui as the double-click handler for installer files.
    Integrate,
    /// Remove the double-click handler registration.
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
        Cmd::List => {
            let items = install::list()?;
            if items.is_empty() {
                println!("Nothing installed with dropin yet.");
                return Ok(());
            }
            for i in items {
                let ext = if i.external { " [external]" } else { "" };
                let ver = i.version.map(|v| format!(" {v}")).unwrap_or_default();
                println!("{}{} — {}{ext}", i.name, ver, i.format, ext = ext);
            }
            Ok(())
        }
        Cmd::Doctor => {
            for (tool, ok, why) in install::doctor() {
                println!("{} {:24} ({why})", if ok { "ok " } else { "MISS" }, tool);
            }
            Ok(())
        }
        Cmd::Integrate => integrate(true),
        Cmd::Unintegrate => integrate(false),
    }
}

/// MIME types dropin-gui claims. Archives are deliberately excluded:
/// data tarballs stay with the archive manager; "Install as application"
/// (M3) covers app archives from the context menu.
const HANDLED_MIMES: &[&str] = &[
    "application/x-iso9660-appimage",
    "application/vnd.debian.binary-package",
    "application/x-rpm",
    "application/x-redhat-package-manager",
    "application/vnd.flatpak.ref",
    "application/vnd.flatpak",
    "application/x-shellscript",
];

fn integrate(enable: bool) -> dropin_core::Result<()> {
    use dropin_core::UserDirs;
    let dirs = UserDirs::new();
    dirs.ensure()?;
    let desktop_path = dirs.desktop_entries.join("dropin-gui.desktop");

    if !enable {
        std::fs::remove_file(&desktop_path).ok();
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&dirs.desktop_entries)
            .output();
        println!("dropin double-click handling removed.");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("dropin-gui")))
        .unwrap_or_else(|| PathBuf::from("dropin-gui"));
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=Install with dropin\n\
         Exec={} %f\nNoDisplay=true\nMimeType={};\nX-Dropin-Managed=true\n",
        exe.display(),
        HANDLED_MIMES.join(";")
    );
    std::fs::write(&desktop_path, desktop)?;

    // Set as the default handler in ~/.config/mimeapps.list.
    let mimeapps = home_config_mimeapps();
    let mut text = std::fs::read_to_string(&mimeapps).unwrap_or_default();
    if !text.contains("[Default Applications]") {
        text.push_str("[Default Applications]\n");
    }
    for mime in HANDLED_MIMES {
        let line = format!("{mime}=dropin-gui.desktop");
        if !text.contains(&line) {
            // Insert right after the [Default Applications] header.
            text = text.replacen(
                "[Default Applications]\n",
                &format!("[Default Applications]\n{line}\n"),
                1,
            );
        }
    }
    if let Some(parent) = mimeapps.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&mimeapps, text)?;

    let _ = std::process::Command::new("update-desktop-database")
        .arg(&dirs.desktop_entries)
        .output();
    println!(
        "dropin will now handle double-clicked AppImages, .deb/.rpm packages, \
         flatpak refs, and installer scripts.\n(Archives still open in your archive manager; \
         right-click → “Install as application” is coming in M3.)"
    );
    Ok(())
}

fn home_config_mimeapps() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dropin_core::home_dir().join(".config"));
    base.join("mimeapps.list")
}

