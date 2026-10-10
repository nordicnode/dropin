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
    // Signature check: pin the issuer key (TOFU) if a detached
    // signature accompanies the source file.
    let signing_key = match crate::signatures::check(src) {
        crate::signatures::SigCheck::Found { fingerprint } => {
            println!("dropin: signature found, key {fingerprint} pinned (trust on first use)");
            Some(fingerprint)
        }
        crate::signatures::SigCheck::None => None,
    };
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
        signing_key,
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
        } else if inst.format == "installer script" {
            // Script installs are fully reversible: undo deletes what the
            // installer added and restores what it changed or deleted.
            match handlers::script::undo_install(&slug) {
                Ok(msg) => println!("{msg}"),
                Err(e) => println!(
                    "note: '{}' was installed by a vendor script; {}",
                    inst.name, e
                ),
            }
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
/// as fallback.
///
/// Generation layout: the delta is applied to a copy in a new version dir,
/// then `active` is flipped. Legacy flat layout: in-place with snapshots.
fn update_appimage(
    inst: &crate::ledger::Install,
    appimage: &str,
    channel: &str,
) -> UpdateReport {
    let name = inst.name.clone();
    let dirs = UserDirs::new();
    let slug = slugify(&inst.name);
    let app_dir = dirs.applications.join(&slug);
    let in_generations = app_dir.is_dir();

    // Resolve the real file (follow `active` if present).
    let local_path = std::path::Path::new(appimage);
    let real_local: std::path::PathBuf = if local_path.is_symlink() {
        std::fs::read_link(local_path)
            .map(|t| if t.is_absolute() { t } else { local_path.parent().unwrap_or(std::path::Path::new(".")).join(t) })
            .unwrap_or_else(|_| local_path.to_path_buf())
    } else {
        local_path.to_path_buf()
    };
    let local = real_local.as_path();

    // For generations: work on a copy in a new version dir.
    let (work_path, new_ver_dir) = if in_generations {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let ver = format!("zsync-{nanos}");
        let vd = app_dir.join(&ver);
        if std::fs::create_dir_all(&vd).is_err() {
            return UpdateReport {
                name,
                status: UpdateStatus::Failed("could not create version directory".into()),
            };
        }
        let file_name = local.file_name().unwrap_or_else(|| std::ffi::OsStr::new("app.AppImage"));
        let wp = vd.join(file_name);
        if std::fs::copy(local, &wp).is_err() {
            return UpdateReport {
                name,
                status: UpdateStatus::Failed("could not copy AppImage for update".into()),
            };
        }
        (wp, Some(vd))
    } else {
        (local.to_path_buf(), None)
    };
    let work_local = work_path.as_path();

    // Native path: resolve the channel, check the published SHA-1, and
    // download only the blocks the local file is missing.
    match crate::zsync::check(work_local, channel) {
        Ok(Some(check)) => {
            if check.up_to_date {
                // Clean up the copy we made.
                if let Some(vd) = new_ver_dir {
                    std::fs::remove_dir_all(&vd).ok();
                }
                return UpdateReport {
                    name,
                    status: UpdateStatus::UpToDate("already current (zsync)".into()),
                };
            }
            let snapshots = dirs.data.join("snapshots").join(&slug);
            match crate::zsync::apply(work_local, &check, &snapshots, &mut |_, _| {}) {
                Ok(report) => {
                    // Generation layout: flip `active` to the new version.
                    if in_generations {
                        let active = app_dir.join("active");
                        if let Err(e) = crate::handlers::appimage::flip_symlink(&active, work_local) {
                            return UpdateReport {
                                name,
                                status: UpdateStatus::Failed(format!("could not activate: {e}")),
                            };
                        }
                        prune_generations(&app_dir, 2);
                    }
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
                    // Clean up the failed copy.
                    if let Some(vd) = new_ver_dir {
                        std::fs::remove_dir_all(&vd).ok();
                    }
                    eprintln!("dropin: native zsync update failed ({e}); trying appimageupdatetool");
                }
            }
        }
        Ok(None) => {
            if let Some(vd) = new_ver_dir {
                std::fs::remove_dir_all(&vd).ok();
            }
        }
        Err(e) => {
            if let Some(vd) = new_ver_dir {
                std::fs::remove_dir_all(&vd).ok();
            }
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

/// Roll back an AppImage to the previous generation.
///
/// With the generation layout, this is just flipping the `active` symlink —
/// instant and atomic. Falls back to the legacy snapshot dir for installs
/// that predate generations.
pub fn rollback(name: &str) -> Result<String> {
    let ledger = Ledger::open()?;
    let inst = ledger.get(&slugify(name))?;
    if inst.format != "AppImage" {
        return Err(Error::Refused(format!(
            "rollback is only for AppImages ({} is {})",
            inst.name, inst.format
        )));
    }
    let dirs = UserDirs::new();
    let slug = slugify(&inst.name);
    let app_dir = dirs.applications.join(&slug);

    // Generation layout: flip `active` to the previous version dir.
    if app_dir.is_dir() {
        let mut vers: Vec<_> = std::fs::read_dir(&app_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        vers.sort();
        if vers.len() >= 2 {
            // Find what `active` currently points at, go to the previous.
            let active = app_dir.join("active");
            let cur_target = std::fs::read_link(&active).ok();
            let cur_ver = cur_target.as_ref()
                .and_then(|t| t.parent())
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str());
            let idx = cur_ver.and_then(|v| vers.iter().position(|p| {
                p.file_name().and_then(|n| n.to_str()) == Some(v)
            }));
            let prev_idx = match idx {
                Some(i) if i > 0 => i - 1,
                _ => vers.len() - 2, // assume latest is active
            };
            let prev_ver = &vers[prev_idx];
            // Find the AppImage inside the previous generation.
            let prev_img = std::fs::read_dir(prev_ver)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("appimage")).unwrap_or(false));
            let Some(prev_img) = prev_img else {
                return Err(Error::NotInstalled("previous generation has no AppImage".to_string()));
            };
            crate::handlers::appimage::flip_symlink(&active, &prev_img)?;
            let ver_name = prev_ver.file_name().and_then(|n| n.to_str()).unwrap_or("previous");
            ledger.clear_version(&slug).ok();
            return Ok(format!("rolled back to generation {ver_name}"));
        }
    }

    // Legacy: snapshot-dir rollback for pre-generation installs.
    let snap_dir = dirs.data.join("snapshots").join(&slug);
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
    let appimage = inst
        .files
        .iter()
        .find(|p| p.to_lowercase().ends_with(".appimage"))
        .ok_or_else(|| Error::NotInstalled(format!("{} has no AppImage file", inst.name)))?;
    let local = std::path::Path::new(appimage);
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
        std::fs::rename(&current_bak, local).ok();
        Error::Io(std::io::Error::other(
            format!("restoring snapshot: {e} (current file preserved)"),
        ))
    })?;
    prune_snapshots(&snap_dir, 2);
    ledger.clear_version(&slug).ok();
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

/// True when rollback can do something: 2+ generations, or legacy snapshots.
/// Used by the GUI to decide whether the Rollback button appears.
pub fn can_rollback(name: &str) -> bool {
    if !snapshots(name).is_empty() {
        return true;
    }
    let dirs = UserDirs::new();
    let app_dir = dirs.applications.join(slugify(name));
    let vers: Vec<_> = std::fs::read_dir(&app_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    vers.len() >= 2
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

    // Signature continuity: if the new download is signed, its key must
    // match the pinned key (or pin it now, TOFU, if none was pinned).
    match crate::signatures::check(&downloaded) {
        crate::signatures::SigCheck::Found { fingerprint } => {
            match &inst.signing_key {
                Some(pinned) if pinned != &fingerprint => {
                    return UpdateReport {
                        name,
                        status: UpdateStatus::Failed(format!(
                            "signing key changed! was {pinned}, now {fingerprint} — refusing update"
                        )),
                    };
                }
                Some(_) => {} // continuity confirmed
                None => {
                    // TOFU: pin it now.
                    if let Ok(l) = Ledger::open() {
                        l.set_signing_key(&slugify(&inst.name), &fingerprint).ok();
                    }
                }
            }
        }
        crate::signatures::SigCheck::None => {}
    }

    // Generation-based update: place the new AppImage in a new version dir,
    // then flip the `active` symlink atomically. The old generation stays
    // for instant rollback; the .desktop entry points at `active` and never
    // needs rewriting.
    let slug = slugify(&inst.name);
    let app_dir = dirs.applications.join(&slug);
    let new_ver = sanitize_version(&release.tag);
    let new_ver_dir = app_dir.join(&new_ver);

    // Migrate old flat-layout installs (~/Applications/<file>.AppImage)
    // into the generation layout on first update.
    if !app_dir.exists() {
        if let Err(e) = migrate_to_generations(inst, &dirs) {
            return UpdateReport {
                name,
                status: UpdateStatus::Failed(format!("could not migrate to generation layout: {e}")),
            };
        }
    }

    if new_ver_dir.exists() {
        return UpdateReport {
            name,
            status: UpdateStatus::Failed(format!("version {} already installed", release.tag)),
        };
    }
    if std::fs::create_dir_all(&new_ver_dir).is_err() {
        return UpdateReport {
            name,
            status: UpdateStatus::Failed("could not create version directory".into()),
        };
    }
    let new_dest = new_ver_dir.join(&asset.name);
    if std::fs::rename(&downloaded, &new_dest).is_err() {
        return UpdateReport {
            name,
            status: UpdateStatus::Failed("could not place new AppImage".into()),
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&new_dest) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = std::fs::set_permissions(&new_dest, perms);
        }
    }

    // Atomic flip. If this fails, the old generation is still active.
    let active = app_dir.join("active");
    if let Err(e) = crate::handlers::appimage::flip_symlink(&active, &new_dest) {
        let _ = std::fs::remove_dir_all(&new_ver_dir);
        return UpdateReport {
            name,
            status: UpdateStatus::Failed(format!("could not activate new version: {e}")),
        };
    }

    // Prune to the last two generations; update the ledger.
    prune_generations(&app_dir, 2);
    let ledger = match Ledger::open() {
        Ok(l) => l,
        Err(e) => {
            return UpdateReport {
                name,
                status: UpdateStatus::Failed(format!("installed but ledger update failed: {e}")),
            }
        }
    };
    ledger.set_github_repo(&slug, Some(repo)).ok();
    ledger.set_version(&slug, &release.tag).ok();
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

/// Sanitize a version tag for use as a directory name.
fn sanitize_version(tag: &str) -> String {
    let s: String = tag
        .trim()
        .trim_start_matches(['v', 'V'])
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "unknown".to_string() } else { s }
}

/// Migrate a flat-layout install (~/Applications/<file>.AppImage) into
/// `~/Applications/<slug>/<version>/` with an `active` symlink.
/// The .desktop entry is rewritten to point at `active`.
fn migrate_to_generations(inst: &crate::ledger::Install, dirs: &UserDirs) -> crate::Result<()> {
    let slug = slugify(&inst.name);
    let app_dir = dirs.applications.join(&slug);
    let current = inst
        .files
        .iter()
        .find(|p| p.to_lowercase().ends_with(".appimage"))
        .ok_or_else(|| crate::Error::NotInstalled(format!("{} has no AppImage file", inst.name)))?;
    let cur_path = std::path::Path::new(current);
    let ver = inst.version.clone().unwrap_or_else(|| "unknown".to_string());
    let ver = sanitize_version(&ver);
    let ver_dir = app_dir.join(&ver);
    std::fs::create_dir_all(&ver_dir)?;
    let file_name = cur_path.file_name().ok_or_else(|| crate::Error::NotInstalled("bad file name".into()))?;
    let new_path = ver_dir.join(file_name);
    std::fs::rename(cur_path, &new_path)?;
    let active = app_dir.join("active");
    crate::handlers::appimage::flip_symlink(&active, &new_path)?;

    // Rewrite the .desktop Exec to point at `active`.
    let desktop = dirs.desktop_entries.join(format!("dropin-{slug}.desktop"));
    if let Ok(text) = std::fs::read_to_string(&desktop) {
        let new_text = text
            .lines()
            .map(|l| {
                if let Some(rest) = l.strip_prefix("Exec=") {
                    let args: Vec<&str> = rest.split_whitespace().collect();
                    if args.is_empty() {
                        format!("Exec={}", active.display())
                    } else {
                        format!("Exec={} {}", active.display(), args[1..].join(" "))
                    }
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&desktop, new_text).ok();
    }

    // Update the ledger: the app dir replaces the flat file.
    let ledger = crate::ledger::Ledger::open()?;
    ledger.replace_file(&slug, current, &app_dir.display().to_string())?;
    Ok(())
}

/// Keep only the newest `keep` generation dirs, oldest first by name.
fn prune_generations(app_dir: &std::path::Path, keep: usize) {
    let mut vers: Vec<_> = std::fs::read_dir(app_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    vers.sort();
    while vers.len() > keep {
        if let Some(old) = vers.first() {
            std::fs::remove_dir_all(old).ok();
        }
        vers.remove(0);
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
