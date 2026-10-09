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
        Format::Rpm => handlers::rpm::plan(src),
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
        Format::Rpm => handlers::rpm::install(src, opts)?,
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
    ledger.record(&crate::ledger::NewInstall {
        name: outcome.name.clone(),
        format: d.format.label().to_string(),
        version: outcome.version.clone(),
        source_path: src.to_path_buf(),
        source_sha256: handlers::sha256_of(src).ok(),
        install_root: outcome.install_root.clone(),
        external: outcome.external,
        update_info: outcome.update_info.clone(),
        files: outcome.paths.clone(),
    })?;
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

/// The result of checking one app for updates.
#[derive(Debug, Clone)]
pub struct UpdateReport {
    pub name: String,
    pub status: UpdateStatus,
}

#[derive(Debug, Clone)]
pub enum UpdateStatus {
    /// Already current (or the updater confirmed nothing to do).
    UpToDate(String),
    /// An update was downloaded and applied.
    Updated(String),
    /// No automated channel; the user must check upstream themselves.
    Manual(String),
    /// Something failed; carries the reason.
    Failed(String),
}

/// Check for updates. `name` selects one app, `all` covers everything.
/// Never runs in the background — only when the user asks.
pub fn update(name: Option<&str>, all: bool) -> Result<Vec<UpdateReport>> {
    let ledger = Ledger::open()?;
    let targets: Vec<crate::ledger::Install> = match (name, all) {
        (Some(n), _) => vec![ledger.get(&slugify(n))?],
        (None, true) => ledger.list()?,
        (None, false) => {
            return Err(Error::Refused(
                "specify an app name or pass --all".into(),
            ))
        }
    };
    Ok(targets.iter().map(update_one).collect())
}

fn update_one(inst: &crate::ledger::Install) -> UpdateReport {
    let name = inst.name.clone();
    // Flatpak owns its updates entirely.
    if inst.format.starts_with("Flatpak") {
        let id = flatpak_id(inst);
        return match crate::run("flatpak", &["update", "--user", "--noninteractive", "-y", &id]) {
            Ok(out) => {
                let msg = out.trim().to_string();
                if msg.contains("Nothing to do") || msg.is_empty() {
                    UpdateReport { name, status: UpdateStatus::UpToDate("flatpak: nothing to do".into()) }
                } else {
                    UpdateReport { name, status: UpdateStatus::Updated("flatpak updated".into()) }
                }
            }
            Err(e) => UpdateReport { name, status: UpdateStatus::Failed(format!("flatpak update failed: {e}")) },
        };
    }
    // AppImage: needs an embedded update channel and the updater tool.
    if inst.format == "AppImage" {
        let channel = inst.update_info.clone().unwrap_or_default();
        if channel.is_empty() {
            return UpdateReport {
                name,
                status: UpdateStatus::Manual(
                    "no update channel embedded in this AppImage — check upstream manually".into(),
                ),
            };
        }
        if !have("appimageupdatetool") {
            return UpdateReport {
                name,
                status: UpdateStatus::Manual(format!(
                    "update channel available ({channel}) but `appimageupdatetool` is not installed"
                )),
            };
        }
        let appimage = inst
            .files
            .iter()
            .find(|p| p.to_lowercase().ends_with(".appimage"));
        let Some(appimage) = appimage else {
            return UpdateReport {
                name,
                status: UpdateStatus::Failed("AppImage file not found in ledger".into()),
            };
        };
        return match crate::run("appimageupdatetool", &["-O", appimage]) {
            Ok(out) => {
                let msg = out.trim().to_string();
                if msg.contains("up to date") || msg.contains("nothing to do") {
                    UpdateReport { name, status: UpdateStatus::UpToDate("already current".into()) }
                } else {
                    UpdateReport { name, status: UpdateStatus::Updated("updated in place".into()) }
                }
            }
            Err(e) => UpdateReport { name, status: UpdateStatus::Failed(format!("appimageupdatetool failed: {e}")) },
        };
    }
    // Everything else: dropin can't know upstream.
    let hint = match inst.format.as_str() {
        s if s.contains("Debian") || s.contains("RPM") => {
            "system packages update via your package manager (`apt upgrade`, `dnf upgrade`)"
        }
        s if s.contains("archive") => "tarballs have no update channel — re-download upstream",
        _ => "no automated update source known — check upstream manually",
    };
    UpdateReport {
        name,
        status: UpdateStatus::Manual(hint.into()),
    }
}

/// Best-effort recovery of the flatpak app id: prefer the Name= from the
/// original .flatpakref, else the slug.
fn flatpak_id(inst: &crate::ledger::Install) -> String {
    std::fs::read_to_string(&inst.source_path)
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.trim().strip_prefix("Name=").map(|s| s.trim().to_string()))
        })
        .unwrap_or_else(|| inst.name.clone())
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
        (
            "appimageupdatetool".into(),
            have("appimageupdatetool"),
            "AppImage updates (optional)".into(),
        ),
    ];
    // Ensure dirs exist as a side effect.
    let _ = dirs.ensure();
    // Mark the first row by whether ensure succeeded.
    rows[0].1 = dirs.data.exists();
    rows
}
