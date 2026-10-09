//! High-level operations: install / remove / list / info / doctor.

use crate::detect::{self, Format};
use crate::handlers::{self, InstallOptions, InstallOutcome, InstallPlan};
use crate::ledger::{remove_path, Ledger};
use crate::{have, slugify, Error, Result, UserDirs};
use std::path::{Path, PathBuf};

pub fn plan_for(src: &Path) -> Result<InstallPlan> {
    let d = detect::detect(src)?;
    match d.format {
        Format::AppImage => handlers::appimage::plan(src),
        Format::Tarball | Format::Zip => handlers::tarball::plan(src),
        Format::Deb => handlers::deb::plan(src),
        Format::Rpm => Err(Error::Unsupported(
            ".rpm support lands in M1.1 (needs rpm2cpio or a Rust cpio reader)".into(),
        )),
        Format::FlatpakRef | Format::FlatpakBundle => handlers::flatpak::plan(src),
        Format::Script => handlers::script::plan(src),
        Format::WindowsExe => Err(Error::Refused(
            "Windows executables need Wine — install Wine and run them directly for now".into(),
        )),
        Format::Unknown => Err(Error::Unsupported("unknown format".into())),
    }
}

pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    let d = detect::detect(src)?;
    let outcome = match d.format {
        Format::AppImage => handlers::appimage::install(src, opts)?,
        Format::Tarball | Format::Zip => handlers::tarball::install(src, opts)?,
        Format::Deb => handlers::deb::install(src, opts)?,
        Format::Rpm => {
            return Err(Error::Unsupported(
                ".rpm support lands in M1.1".into(),
            ))
        }
        Format::FlatpakRef | Format::FlatpakBundle => handlers::flatpak::install(src, opts)?,
        Format::Script => handlers::script::install(src, opts)?,
        Format::WindowsExe => {
            return Err(Error::Refused(
                "Windows executables need Wine — install Wine and run them directly for now".into(),
            ))
        }
        Format::Unknown => return Err(Error::Unsupported("unknown format".into())),
    };
    let ledger = Ledger::open()?;
    let sha = handlers::sha256_of(src).ok();
    ledger.record(
        &outcome.name,
        d.format.label(),
        outcome.version.as_deref(),
        src,
        sha.as_deref(),
        outcome.install_root.as_deref(),
        outcome.external,
        &outcome.paths,
    )?;
    Ok(outcome)
}

/// Remove an installed app, reversing everything the ledger recorded.
pub fn remove(name: &str) -> Result<()> {
    let slug = slugify(name);
    let ledger = Ledger::open()?;
    let inst = ledger.get(&slug)?;

    if inst.external {
        // Delegate to the owning system.
        if inst.format.starts_with("Flatpak") {
            // Ledger name is the slugified app id; the original id had dots.
            // Try the source ref first, then fall back.
            let id = std::fs::read_to_string(&inst.source_path)
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find_map(|l| l.trim().strip_prefix("Name=").map(|s| s.trim().to_string()))
                })
                .unwrap_or_else(|| slug.clone());
            handlers::flatpak::remove_external(&id)?;
        } else {
            println!(
                "note: '{}' was installed externally ({}); dropin removed its ledger entry, but files installed by the vendor script / system package manager remain — use that tool to fully uninstall",
                inst.name, inst.format
            );
        }
        ledger.forget(&slug)?;
        return Ok(());
    }

    // Delete deepest paths first so parents are empty when removed.
    let mut paths: Vec<PathBuf> = inst.files.iter().map(PathBuf::from).collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    // The install root contains everything beneath it; removing the root
    // covers children, but we still walk the list for stragglers (symlinks,
    // .desktop entries, icons live outside the root).
    for p in &paths {
        if let Err(e) = remove_path(p) {
            eprintln!("warning: could not remove {}: {e}", p.display());
        }
    }
    ledger.forget(&slug)?;
    Ok(())
}

pub fn list() -> Result<Vec<crate::ledger::Install>> {
    Ledger::open()?.list()
}

/// Environment sanity check: which helper tools are present?
pub fn doctor() -> Vec<(String, bool, String)> {
    let dirs = UserDirs::new();
    let mut rows = vec![
        (
            "home directories".into(),
            true,
            format!("managed under {}", dirs.data.display()),
        ),
        ("tar".into(), have("tar"), "archive handling".into()),
        ("unzip".into(), have("unzip"), "zip handling".into()),
        ("ar".into(), have("ar"), ".deb handling".into()),
        ("flatpak".into(), have("flatpak"), "flatpak handling".into()),
        (
            "sha256sum".into(),
            have("sha256sum"),
            "integrity hashes".into(),
        ),
        (
            "update-desktop-database".into(),
            have("update-desktop-database"),
            "menu refresh".into(),
        ),
        ("wine".into(), have("wine"), ".exe handoff (optional)".into()),
    ];
    // Ensure dirs exist as a side effect.
    let _ = dirs.ensure();
    // Mark the first row by whether ensure succeeded.
    rows[0].1 = dirs.data.exists();
    rows
}
