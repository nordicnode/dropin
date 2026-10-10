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
    // Integrity check before anything touches the system: a companion
    // checksum that doesn't match means a corrupt or tampered download.
    let sha256 = handlers::sha256_of(src).ok();
    let verified = match sha256.as_deref() {
        Some(sha) => match crate::verify::verify(src, sha) {
            crate::verify::Verification::Verified { checksum_file } => {
                println!("integrity: SHA-256 verified (from {checksum_file})");
                true
            }
            crate::verify::Verification::Unverified => false,
            crate::verify::Verification::Mismatch {
                expected,
                actual,
                checksum_file,
            } => {
                return Err(Error::Refused(format!(
                    "checksum mismatch — refusing to install.\n  expected ({checksum_file}): {expected}\n  actual:                    {actual}\nThis usually means a corrupt or tampered download; re-download and try again."
                )));
            }
        },
        None => false,
    };
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
        source_sha256: sha256,
        install_root: outcome.install_root.clone(),
        external: outcome.external,
        update_info: outcome.update_info.clone(),
        verified,
        github_repo: None,
        files: outcome.paths.clone(),
    })?;
    Ok(outcome)
}

/// Remove an installed app, reversing everything the ledger recorded.
/// With `purge`, also moves leftover config/cache/data dirs to the trash
/// (conservative heuristics; never deletes outright).
pub fn remove(name: &str, purge: bool) -> Result<()> {
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

    if purge && !inst.external {
        let remaining: Vec<crate::ledger::Install> =
            ledger.list()?.into_iter().filter(|i| i.name != slug).collect();
        if crate::health::name_still_provided(&remaining, &inst) {
            println!(
                "note: skipping leftover purge — '{}' is still provided by another install or system package",
                inst.name
            );
        } else {
            let leftovers = crate::cleanup::find_leftovers(&inst);
            if leftovers.is_empty() {
                println!("no leftover config/cache/data dirs found");
            } else {
                let trashed = crate::cleanup::trash_leftovers(&leftovers)?;
                println!("moved {} leftover dir(s) to the trash:", trashed.len());
                for l in &leftovers {
                    println!("  {}", l.what);
                }
            }
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
/// The GUI manager may run this automatically on launch (user-configurable);
/// the CLI only runs it when asked.
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
    // AppImage: an explicitly tracked GitHub repo wins; otherwise native
    // zsync first, appimageupdatetool as fallback.
    if inst.format == "AppImage" {
        if let Some(repo) = inst.github_repo.clone() {
            return update_github(inst, &repo);
        }
        let channel = inst.update_info.clone().unwrap_or_default();
        if channel.is_empty() {
            return UpdateReport {
                name,
                status: UpdateStatus::Manual(
                    "no update channel embedded in this AppImage — check upstream manually".into(),
                ),
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
        return update_appimage(inst, appimage, &channel);
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

/// Update one AppImage: native zsync delta first, `appimageupdatetool`
/// as fallback. The old AppImage is snapshotted before any swap, so a bad
/// update can be rolled back with `dropin rollback`.
fn update_appimage(
    inst: &crate::ledger::Install,
    appimage: &str,
    channel: &str,
) -> UpdateReport {
    let name = inst.name.clone();
    let local = std::path::Path::new(appimage);

    // Native path: resolve the channel, check the published SHA-1, and
    // download only the blocks the local file is missing.
    match crate::zsync::check(local, channel) {
        Ok(Some(check)) => {
            if check.up_to_date {
                return UpdateReport {
                    name,
                    status: UpdateStatus::UpToDate("already current (zsync)".into()),
                };
            }
            let dirs = UserDirs::new();
            let snapshots = dirs.data.join("snapshots").join(slugify(&inst.name));
            match crate::zsync::apply(local, &check, &snapshots, &mut |_, _| {}) {
                Ok(report) => {
                    return UpdateReport {
                        name,
                        status: UpdateStatus::Updated(format!(
                            "zsync delta: {:.1} MiB fetched, {}/{} blocks reused",
                            report.downloaded_bytes as f64 / 1048576.0,
                            report.reused_blocks,
                            report.total_blocks,
                        )),
                    };
                }
                Err(e) => {
                    // Fall through to appimageupdatetool below; the local
                    // file is untouched (apply only swaps after verifying).
                    eprintln!("dropin: native zsync update failed ({e}); trying appimageupdatetool");
                }
            }
        }
        Ok(None) => {
            // Channel type dropin doesn't speak natively; fall through.
        }
        Err(e) => {
            eprintln!("dropin: zsync check failed ({e}); trying appimageupdatetool");
        }
    }

    if !have("appimageupdatetool") {
        return UpdateReport {
            name,
            status: UpdateStatus::Manual(format!(
                "update channel available ({channel}) but neither native zsync nor `appimageupdatetool` could handle it"
            )),
        };
    }
    match crate::run("appimageupdatetool", &["-O", appimage]) {
        Ok(out) => {
            let msg = out.trim().to_string();
            if msg.contains("up to date") || msg.contains("nothing to do") {
                UpdateReport { name, status: UpdateStatus::UpToDate("already current".into()) }
            } else {
                UpdateReport { name, status: UpdateStatus::Updated("updated in place".into()) }
            }
        }
        Err(e) => UpdateReport { name, status: UpdateStatus::Failed(format!("appimageupdatetool failed: {e}")) },
    }
}

/// Restore the most recent pre-update snapshot of an AppImage.
///
/// The file being replaced is itself snapshotted first, so a rollback can
/// be rolled back. Only AppImages get snapshots (they are the only format
/// dropin updates in place).
pub fn rollback(name: &str) -> Result<String> {
    let ledger = Ledger::open()?;
    let inst = ledger.get(&slugify(name))?;
    if inst.format != "AppImage" {
        return Err(Error::Refused(format!(
            "rollback is only for AppImages ({} is {})",
            inst.name, inst.format
        )));
    }
    let appimage = inst
        .files
        .iter()
        .find(|p| p.to_lowercase().ends_with(".appimage"))
        .ok_or_else(|| Error::NotInstalled(format!("{} has no AppImage file", inst.name)))?;
    let local = std::path::Path::new(appimage);

    let dirs = UserDirs::new();
    let snap_dir = dirs.data.join("snapshots").join(slugify(&inst.name));
    let mut snaps: Vec<_> = std::fs::read_dir(&snap_dir)
        .map_err(|_| Error::NotInstalled(format!("no snapshots for {}", inst.name)))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("bak"))
        .collect();
    if snaps.is_empty() {
        return Err(Error::NotInstalled(format!("no snapshots for {}", inst.name)));
    }
    snaps.sort();
    let latest = snaps.pop().unwrap();

    // Snapshot the current file first: rolling back must not destroy it.
    // Nanosecond resolution: two snapshots in the same second must not
    // overwrite each other.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = local.file_name().and_then(|n| n.to_str()).unwrap_or("appimage");
    let current_bak = snap_dir.join(format!("{file_name}-{nanos}.bak"));
    if local.exists() {
        std::fs::rename(local, &current_bak)?;
    }
    std::fs::rename(&latest, local).map_err(|e| {
        // Try to restore; report honestly if that fails too.
        std::fs::rename(&current_bak, local).ok();
        Error::Io(std::io::Error::other(
            format!("restoring snapshot: {e} (current file preserved)"),
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(local)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(local, perms)?;
    }
    // Keep the snapshot count bounded: two is enough to go back and forth.
    prune_snapshots(&snap_dir, 2);

    Ok(format!(
        "rolled back to {}",
        latest.file_name().and_then(|n| n.to_str()).unwrap_or("snapshot")
    ))
}

/// List snapshot files for an app, oldest first. Used by the GUI to decide
/// whether "Rollback" is available.
pub fn snapshots(name: &str) -> Vec<std::path::PathBuf> {
    let dirs = UserDirs::new();
    let snap_dir = dirs.data.join("snapshots").join(slugify(name));
    let mut snaps: Vec<_> = std::fs::read_dir(&snap_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("bak"))
        .collect();
    snaps.sort();
    snaps
}

/// Keep at most `keep` snapshots in `dir`, oldest first.
fn prune_snapshots(dir: &std::path::Path, keep: usize) {
    let mut snaps: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("bak"))
        .collect();
    snaps.sort();
    while snaps.len() > keep {
        let old = snaps.remove(0);
        std::fs::remove_file(old).ok();
    }
}

/// Associate a GitHub repo with an installed AppImage for release tracking.
pub fn track(name: &str, github_repo: &str) -> Result<String> {
    let repo = github_repo.trim().trim_end_matches('/');
    if repo.split('/').count() != 2 || repo.split('/').any(|p| p.is_empty()) {
        return Err(Error::Refused(format!(
            "{github_repo:?} is not an owner/repo"
        )));
    }
    let ledger = Ledger::open()?;
    let inst = ledger.get(&slugify(name))?;
    if inst.format != "AppImage" {
        return Err(Error::Refused(format!(
            "GitHub release tracking is for AppImages ({} is {})",
            inst.name, inst.format
        )));
    }
    ledger.set_github_repo(&inst.name, Some(repo))?;
    Ok(format!("tracking {} for {}", inst.name, repo))
}

/// Stop tracking a GitHub repo for an app.
pub fn untrack(name: &str) -> Result<String> {
    let ledger = Ledger::open()?;
    let inst = ledger.get(&slugify(name))?;
    match &inst.github_repo {
        Some(repo) => {
            ledger.set_github_repo(&inst.name, None)?;
            Ok(format!("no longer tracking {} ({})", inst.name, repo))
        }
        None => Ok(format!("{} was not tracking a GitHub repo", inst.name)),
    }
}

/// Update an AppImage from its tracked GitHub repo: compare the latest
/// release tag against the installed version, download the new AppImage
/// asset, snapshot the old file, and install the new one.
fn update_github(inst: &crate::ledger::Install, repo: &str) -> UpdateReport {
    let name = inst.name.clone();
    let dirs = UserDirs::new();

    let (release, _cached) = match crate::github::latest_release(repo, &dirs.data) {
            Ok(r) => r,
            Err(e) => {
                return UpdateReport {
                    name,
                    status: UpdateStatus::Failed(format!("GitHub release check failed: {e}")),
                }
            }
        };

    if !crate::github::is_newer(inst.version.as_deref(), &release.tag) {
        return UpdateReport {
            name,
            status: UpdateStatus::UpToDate(format!(
                "{} already current",
                inst.version.as_deref().unwrap_or(&release.tag)
            )),
        };
    }

    let asset = match crate::github::pick_appimage_asset(&release) {
        Some(a) => a.clone(),
        None => {
            return UpdateReport {
                name,
                status: UpdateStatus::Failed(format!(
                    "release {} has no AppImage asset for this machine",
                    release.tag
                )),
            }
        }
    };

    // Download the new AppImage.
    let tmpdir = std::env::temp_dir().join(format!("dropin-gh-{}", std::process::id()));
    if std::fs::create_dir_all(&tmpdir).is_err() {
        return UpdateReport {
            name,
            status: UpdateStatus::Failed("could not create download directory".into()),
        };
    }
    let downloaded = tmpdir.join(&asset.name);
    if let Err(e) = crate::zsync::fetch::download_to_file(
        &asset.download_url,
        &downloaded,
        2 * 1024 * 1024 * 1024,
    ) {
        return UpdateReport {
            name,
            status: UpdateStatus::Failed(format!("download failed: {e}")),
        };
    }

    // Snapshot the current AppImage, then get it out of the way so the
    // normal install flow can place the new one.
    let current = inst
        .files
        .iter()
        .find(|p| p.to_lowercase().ends_with(".appimage"))
        .cloned();
    if let Some(cur) = &current {
        let snap_dir = dirs.data.join("snapshots").join(slugify(&inst.name));
        if std::fs::create_dir_all(&snap_dir).is_ok() {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let bak = snap_dir.join(format!(
                "{}-{nanos}.bak",
                std::path::Path::new(cur)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("appimage")
            ));
            std::fs::rename(cur, &bak).ok();
        }
    }

    // Install the new AppImage through the normal flow (menu integration
    // and all), then restore tracking state.
    let opts = InstallOptions { keep_source: true, ..Default::default() };
    match install(&downloaded, &opts) {
        Ok(out) => {
            let ledger = match Ledger::open() {
                Ok(l) => l,
                Err(e) => {
                    return UpdateReport {
                        name,
                        status: UpdateStatus::Failed(format!("installed but ledger update failed: {e}")),
                    }
                }
            };
            // The new AppImage may detect under a different name than the
            // entry we updated from. Keep the user's original name: drop the
            // stale entry and rename the new one, so no duplicate appears.
            // The old .desktop/icon files are removed (the AppImage itself
            // was already snapshotted above).
            let orig_slug = slugify(&name);
            if out.name != inst.name {
                for f in &inst.files {
                    if f.to_lowercase().ends_with(".appimage") {
                        continue;
                    }
                    std::fs::remove_file(f).ok();
                }
                ledger.forget(&inst.name).ok();
                ledger.rename(&out.name, &inst.name).ok();
            }
            ledger.set_github_repo(&orig_slug, Some(repo)).ok();
            ledger.set_version(&orig_slug, &release.tag).ok();
            std::fs::remove_dir_all(&tmpdir).ok();
            UpdateReport {
                name,
                status: UpdateStatus::Updated(format!(
                    "GitHub: {} -> {} ({})",
                    inst.version.as_deref().unwrap_or("unknown"),
                    release.tag,
                    asset.name
                )),
            }
        }
        Err(e) => {
            // Try to restore the snapshotted file.
            if let Some(cur) = current {
                let snap_dir = dirs.data.join("snapshots").join(slugify(&inst.name));
                if let Ok(mut snaps) = std::fs::read_dir(&snap_dir).map(|r| {
                    r.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("bak"))
                        .collect::<Vec<_>>()
                }) {
                    snaps.sort();
                    if let Some(latest) = snaps.pop() {
                        std::fs::rename(&latest, &cur).ok();
                    }
                }
            }
            UpdateReport {
                name,
                status: UpdateStatus::Failed(format!("installing new AppImage failed: {e}")),
            }
        }
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
