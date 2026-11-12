//! High-level operations: install / remove / list / info / doctor.

use crate::detect::{self, Format};
use crate::handlers::{self, InstallOptions, InstallOutcome, InstallPlan};
use crate::ledger::{remove_path, Ledger};
use crate::{have, slugify, Error, Result, UserDirs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Removes a temporary directory when it goes out of scope, including on
/// early returns and errors.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Human-readable format label, including the specific archive type
/// (e.g. "tar.gz archive" rather than just "compressed archive").
pub fn format_label(d: &detect::Detection, src: &Path) -> String {
    let base = d.format.label();
    if matches!(d.format, Format::Tarball | Format::Zip) {
        let fname = src.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fname.ends_with(".tar.gz") {
            "tar.gz archive".into()
        } else if fname.ends_with(".tar.bz2") {
            "tar.bz2 archive".into()
        } else if fname.ends_with(".tar.xz") {
            "tar.xz archive".into()
        } else if fname.ends_with(".tar.zst") {
            "tar.zst archive".into()
        } else if fname.ends_with(".tar") {
            "tar archive".into()
        } else if let Some(ext) = src.extension().and_then(|e| e.to_str()) {
            format!("{ext} archive")
        } else {
            base.to_string()
        }
    } else {
        base.to_string()
    }
}

pub fn plan_for(src: &Path) -> Result<InstallPlan> {
    let d = detect::detect(src)?;
    match d.format {
        Format::AppImage => handlers::appimage::plan(src),
        Format::Tarball | Format::Zip => handlers::tarball::plan(src),
        Format::Deb => handlers::deb::plan(src),
        Format::Rpm => handlers::rpm::plan(src),
        Format::FlatpakRef | Format::FlatpakBundle => handlers::flatpak::plan(src),
        Format::Script => handlers::script::plan(src),
        Format::WindowsExe => handlers::wine::plan(src),
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
    // Signature check before anything is installed: pin the issuer key
    // (TOFU) if a detached signature accompanies the source file. A
    // signature that exists but can't be parsed is reported, not ignored.
    let signing_key = match crate::signatures::check(src) {
        crate::signatures::SigCheck::Found {
            fingerprint,
            verified: true,
        } => {
            println!("dropin: signature verified with gpg (key {fingerprint})");
            Some(fingerprint)
        }
        crate::signatures::SigCheck::Found {
            fingerprint,
            verified: false,
        } => {
            println!(
                "dropin: signature found, issuer key {fingerprint} pinned (continuity only; install gpg and the public key to verify)"
            );
            Some(fingerprint)
        }
        crate::signatures::SigCheck::Bad(why) => {
            return Err(Error::Refused(format!(
                "signature check failed — refusing to install.\n  {why}\nThis usually means the download was modified after signing; re-download it."
            )));
        }
        crate::signatures::SigCheck::Invalid(why) => {
            eprintln!("dropin: warning: a signature file is present but could not be read ({why}); continuing without pinning");
            None
        }
        crate::signatures::SigCheck::None => None,
    };
    let outcome = match d.format {
        Format::AppImage => handlers::appimage::install(src, opts)?,
        Format::Tarball | Format::Zip => handlers::tarball::install(src, opts)?,
        Format::Deb => handlers::deb::install(src, opts)?,
        Format::Rpm => handlers::rpm::install(src, opts)?,
        Format::FlatpakRef | Format::FlatpakBundle => handlers::flatpak::install(src, opts)?,
        Format::Script => handlers::script::install(src, opts)?,
        Format::WindowsExe => handlers::wine::install(src, opts)?,
        Format::Unknown => return Err(Error::Unsupported("unknown format".into())),
    };
    let ledger = Ledger::open()?;
    ledger.record(&crate::ledger::NewInstall {
        name: outcome.name.clone(),
        format: format_label(&d, src),
        version: outcome.version.clone(),
        source_path: src.to_path_buf(),
        source_sha256: sha256,
        install_root: outcome.install_root.clone(),
        external: outcome.external,
        update_info: outcome.update_info.clone(),
        verified,
        github_repo: None,
        signing_key,
        env_vars: None,
        files: outcome.paths.clone(),
    })?;
    Ok(outcome)
}

/// Remove an installed app, reversing everything the ledger recorded.
/// With `purge`, also moves leftover config/cache/data dirs to the trash
/// (conservative heuristics; never deletes outright).
pub fn remove(name: &str, purge: bool) -> Result<String> {
    let slug = slugify(name);
    let ledger = Ledger::open()?;
    let inst = ledger.get(&slug)?;

    if inst.external {
        // Delegate to the owning system.
        let msg = if inst.format.starts_with("Flatpak") {
            // Prefer the real app id stored at install time; fall back to the
            // source .flatpakref, then the slug.
            let id = inst
                .update_info
                .clone()
                .or_else(|| {
                    std::fs::read_to_string(&inst.source_path)
                        .ok()
                        .and_then(|t| {
                            t.lines().find_map(|l| {
                                l.trim().strip_prefix("Name=").map(|s| s.trim().to_string())
                            })
                        })
                })
                .unwrap_or_else(|| slug.clone());
            handlers::flatpak::remove_external(&id)?;
            format!("Removed '{}'.", inst.name)
        } else if inst.format == "installer script" {
            // Script installs are fully reversible: undo deletes what the
            // installer added and restores what it changed or deleted.
            let m = match handlers::script::undo_install(&slug) {
                Ok(m) => m,
                Err(e) => format!(
                    "Removed '{}' from the list, but couldn't undo everything: {e}",
                    inst.name
                ),
            };
            if purge {
                purge_leftovers(&ledger, &inst, &slug)?;
            }
            m
        } else if inst.format == "adopted" || (inst.external && inst.format == "Windows executable")
        {
            // Adopted games: delete dropin's own files (.desktop, bin symlink,
            // wine prefix) but leave the user's game files in place.
            let dirs = UserDirs::new();
            let mut removed = 0;
            for f in &inst.files {
                let p = PathBuf::from(f);
                // Only delete files in dropin's managed locations.
                let in_managed = p.starts_with(&dirs.desktop_entries)
                    || p.starts_with(&dirs.bin)
                    || p.starts_with(dirs.data.join("wine-prefixes"));
                if in_managed && remove_path(&p).is_ok() {
                    removed += 1;
                }
            }
            let game_dir = inst
                .install_root
                .as_ref()
                .map(|r| PathBuf::from(r).display().to_string())
                .unwrap_or_else(|| "its folder".to_string());
            format!(
                "Removed '{}' from dropin (deleted {removed} launcher file{}). Your game files in {} were left in place.",
                inst.name,
                if removed == 1 { "" } else { "s" },
                game_dir
            )
        } else if inst.format.contains("Debian") || inst.format.contains("RPM") {
            // Installed with --system: the package manager owns the files,
            // so remove it through the same package manager. Prefer the real
            // package name stored at install time over the slug.
            let pkg = inst.update_info.as_deref().unwrap_or(&inst.name);
            match handlers::remove_system_package(pkg) {
                Ok(m) => m,
                Err(e) => format!(
                    "Removed '{}' from the list, but the system package could not be removed: {e}",
                    inst.name
                ),
            }
        } else {
            format!(
                "'{}' was installed externally ({}); removed its ledger entry, but files installed by the vendor script / system package manager remain — use that tool to fully uninstall",
                inst.name, inst.format
            )
        };
        ledger.forget(&slug)?;
        return Ok(msg);
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

    if purge {
        purge_leftovers(&ledger, &inst, &slug)?;
    }

    ledger.forget(&slug)?;
    Ok(format!("Removed '{}'.", inst.name))
}

/// Move leftover config/cache/data dirs to the trash, unless the app is still
/// provided by another install or a system package.
fn purge_leftovers(ledger: &Ledger, inst: &crate::ledger::Install, slug: &str) -> Result<()> {
    let remaining: Vec<crate::ledger::Install> = ledger
        .list()?
        .into_iter()
        .filter(|i| i.name != slug)
        .collect();
    if crate::health::name_still_provided(&remaining, inst) {
        println!(
            "note: skipping leftover purge — '{}' is still provided by another install or system package",
            inst.name
        );
        return Ok(());
    }
    let leftovers = crate::cleanup::find_leftovers(inst);
    if leftovers.is_empty() {
        println!("no leftover config/cache/data dirs found");
    } else {
        let trashed = crate::cleanup::trash_leftovers(&leftovers)?;
        println!("moved {} leftover dir(s) to the trash:", trashed.len());
        for l in &leftovers {
            println!("  {}", l.what);
        }
    }
    Ok(())
}

pub fn list() -> Result<Vec<crate::ledger::Install>> {
    Ledger::open()?.list()
}

/// Launch an installed app with its configured environment variables.
///
/// Finds the executable from the desktop entry (preferred) or the
/// installed files, then spawns it detached.
/// Split a desktop entry Exec line, respecting double quotes.
fn split_exec(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    parts.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

pub fn launch(name: &str) -> Result<String> {
    let slug = slugify(name);
    let ledger = Ledger::open()?;
    let inst = ledger.get(&slug)?;
    let dirs = UserDirs::new();

    // Find the executable: prefer a managed desktop entry's Exec line.
    // The entry is not always named dropin-<slug>.desktop (a .deb can ship
    // several, e.g. debian-xterm.desktop), so look at the tracked files.
    let mut exe: Option<PathBuf> = None;
    let mut args: Vec<String> = Vec::new();
    if let Some(desktop) = find_managed_desktop(&inst, &dirs, &slug) {
        if let Ok(text) = std::fs::read_to_string(&desktop) {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("Exec=") {
                    let parts = split_exec(rest);
                    if !parts.is_empty() {
                        // Skip field codes (%f, %F, %u, %U, etc.).
                        let clean: Vec<String> =
                            parts.into_iter().filter(|p| !p.starts_with('%')).collect();
                        if let Some(e) = clean.first() {
                            exe = Some(PathBuf::from(e));
                            args = clean[1..].to_vec();
                        }
                    }
                    break;
                }
            }
        }
    }
    // Resolve a bare command name (Exec=myapp) to a real path: the dropin
    // bin dir first, then PATH. Without this, a bundled .desktop that says
    // `Exec=myapp` fails even though dropin linked myapp into ~/.local/bin.
    if let Some(e) = exe.take() {
        exe = resolve_command(&e, &dirs.bin);
    }
    // Fallback: find an executable in the tracked files, preferring one
    // whose name matches the app (a .deb can ship several helpers).
    if exe.is_none() {
        let mut executables: Vec<PathBuf> = inst
            .files
            .iter()
            .map(PathBuf::from)
            .filter(|p| {
                p.is_file()
                    && std::fs::metadata(p)
                        .map(|m| m.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false)
            })
            .collect();
        executables.sort_by_key(|p| {
            let stem = p
                .file_stem()
                .map(|s| slugify(&s.to_string_lossy()))
                .unwrap_or_default();
            let name_match = stem == slug || slug.contains(&stem) || stem.contains(&slug);
            (!name_match, p.components().count())
        });
        exe = executables.into_iter().next();
    }
    // Fallback for AppImages: the active symlink.
    if exe.is_none() && inst.format == "AppImage" {
        let active = dirs.applications.join(&slug).join("active");
        if active.exists() {
            exe = Some(active);
        }
    }
    let exe = exe.ok_or_else(|| Error::NotInstalled(format!("no executable found for {name}")))?;

    // Apply per-app environment variables.
    let env = ledger.env_vars(&slug).unwrap_or_default();

    // For ELF binaries: check for missing shared libraries before launching.
    // This catches the common "error while loading shared libraries" failure.
    if is_elf(&exe) {
        if let Ok(output) = std::process::Command::new("ldd").arg(&exe).output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let missing: Vec<&str> = stdout
                    .lines()
                    .filter(|l| l.contains("not found"))
                    .filter_map(|l| l.split_whitespace().next())
                    .collect();
                if !missing.is_empty() {
                    return Err(Error::Command(format!(
                        "Couldn't launch '{}': missing shared libraries: {}. Install them via your system package manager, then try again.",
                        name,
                        missing.join(", ")
                    )));
                }
            }
        }
    }

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    // Detach: don't inherit stdio, but keep stderr piped briefly to catch
    // immediate crashes.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Command(format!("couldn't launch {name}: {e}")))?;

    // Wait briefly to catch immediate crashes (e.g., missing libs, bad exec).
    // If the process exits within 500ms, report the error instead of "Launched".
    std::thread::sleep(std::time::Duration::from_millis(500));
    match child.try_wait() {
        Ok(Some(status)) => {
            // Process exited immediately. A clean exit (status 0) is normal
            // for CLI tools and apps that fork a GUI and return; only a
            // non-zero status means the launch actually failed.
            if status.success() {
                return Ok(format!("Launched '{name}' (exited)."));
            }
            let mut stderr = String::new();
            if let Some(mut err) = child.stderr.take() {
                use std::io::Read;
                let _ = err.read_to_string(&mut stderr);
            }
            let code = status.code().unwrap_or(-1);
            let err_msg = if stderr.trim().is_empty() {
                format!("exited immediately with status {code}")
            } else {
                format!(
                    "exited immediately with status {code}: {}",
                    stderr.trim().lines().next().unwrap_or("")
                )
            };
            return Err(Error::Command(format!(
                "Couldn't launch '{name}': {err_msg}."
            )));
        }
        Ok(None) => {
            // Still running: detach and report success.
            // (We leave the child running; the OS reaps it on exit.)
        }
        Err(e) => {
            return Err(Error::Command(format!("Couldn't launch '{name}': {e}")));
        }
    }
    Ok(format!("Launched '{name}'."))
}

/// Find the managed desktop entry for an install. Prefers an entry whose
/// filename or `Name=` matches the app, then the conventional
/// `dropin-<slug>.desktop`, then any tracked entry.
fn find_managed_desktop(
    inst: &crate::ledger::Install,
    dirs: &UserDirs,
    slug: &str,
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = inst
        .files
        .iter()
        .map(PathBuf::from)
        .filter(|p| {
            p.starts_with(&dirs.desktop_entries)
                && p.extension().map(|e| e == "desktop").unwrap_or(false)
        })
        .collect();
    if candidates.is_empty() {
        let conventional = dirs.desktop_entries.join(format!("dropin-{slug}.desktop"));
        return conventional.exists().then_some(conventional);
    }
    candidates.sort_by_key(|p| (desktop_match_score(p, slug), p.components().count()));
    candidates.into_iter().next()
}

/// Lower is better: 0 = the entry's Exec binary or Name matches the app,
/// 1 = a partial name match, 2 = unrelated.
fn desktop_match_score(path: &Path, slug: &str) -> u8 {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return 2,
    };
    let mut exec_stem = String::new();
    let mut display = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Exec=") {
            if let Some(first) = split_exec(rest).first() {
                exec_stem = Path::new(first)
                    .file_stem()
                    .map(|s| slugify(&s.to_string_lossy()))
                    .unwrap_or_default();
            }
        } else if let Some(rest) = line.strip_prefix("Name=") {
            display = slugify(rest.trim());
        }
    }
    if exec_stem == slug || display == slug {
        0
    } else if exec_stem.contains(slug)
        || slug.contains(&exec_stem)
        || display.contains(slug)
        || slug.contains(&display)
    {
        1
    } else {
        2
    }
}

/// Resolve a command from a desktop entry to a real path.
/// Absolute paths and paths with a directory component are used as-is;
/// bare names are looked up in dropin's bin dir, then `PATH`.
fn resolve_command(cmd: &Path, bin_dir: &Path) -> Option<PathBuf> {
    if cmd.is_absolute() || cmd.components().count() > 1 {
        return cmd.exists().then(|| cmd.to_path_buf());
    }
    let local = bin_dir.join(cmd);
    if local.is_file() {
        return Some(local);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(cmd);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Check if a file is an ELF binary (by magic bytes).
fn is_elf(path: &Path) -> bool {
    let mut buf = [0u8; 4];
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open(path) {
        if f.read_exact(&mut buf).is_ok() {
            return buf == [0x7f, b'E', b'L', b'F'];
        }
    }
    false
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
        (None, false) => return Err(Error::Refused("specify an app name or pass --all".into())),
    };
    Ok(targets.iter().map(update_one).collect())
}

/// Check for available updates without applying them.
/// Returns the names of apps that have updates available.
pub fn check_for_updates() -> Result<Vec<String>> {
    let ledger = Ledger::open()?;
    let mut available = Vec::new();
    for inst in ledger.list()? {
        if let Some(repo) = &inst.github_repo {
            let dirs = UserDirs::new();
            if let Ok((release, _)) = crate::github::latest_release(repo, &dirs.data) {
                if crate::github::is_newer(inst.version.as_deref(), &release.tag) {
                    available.push(inst.name.clone());
                }
            }
        }
    }
    Ok(available)
}

fn update_one(inst: &crate::ledger::Install) -> UpdateReport {
    let name = inst.name.clone();
    // Flatpak owns its updates entirely.
    if inst.format.starts_with("Flatpak") {
        let id = flatpak_id(inst);
        return match crate::run(
            "flatpak",
            &["update", "--user", "--noninteractive", "-y", &id],
        ) {
            Ok(out) => {
                let msg = out.trim().to_string();
                if msg.contains("Nothing to do") || msg.is_empty() {
                    UpdateReport {
                        name,
                        status: UpdateStatus::UpToDate("flatpak: nothing to do".into()),
                    }
                } else {
                    UpdateReport {
                        name,
                        status: UpdateStatus::Updated("flatpak updated".into()),
                    }
                }
            }
            Err(e) => UpdateReport {
                name,
                status: UpdateStatus::Failed(format!("flatpak update failed: {e}")),
            },
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
        "installer script" => {
            "vendor scripts have no update channel — download the new installer and run it"
        }
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
fn update_appimage(inst: &crate::ledger::Install, appimage: &str, channel: &str) -> UpdateReport {
    let name = inst.name.clone();
    let dirs = UserDirs::new();
    let slug = slugify(&inst.name);
    let app_dir = dirs.applications.join(&slug);
    let in_generations = app_dir.is_dir();

    // Resolve the real file (follow `active` if present).
    let local_path = std::path::Path::new(appimage);
    let real_local: std::path::PathBuf = if local_path.is_symlink() {
        std::fs::read_link(local_path)
            .map(|t| {
                if t.is_absolute() {
                    t
                } else {
                    local_path
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .join(t)
                }
            })
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
        let file_name = local
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("app.AppImage"));
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
                        if let Err(e) = crate::handlers::appimage::flip_symlink(&active, work_local)
                        {
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
                    eprintln!(
                        "dropin: native zsync update failed ({e}); trying appimageupdatetool"
                    );
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
                UpdateReport {
                    name,
                    status: UpdateStatus::UpToDate("already current".into()),
                }
            } else {
                UpdateReport {
                    name,
                    status: UpdateStatus::Updated("updated in place".into()),
                }
            }
        }
        Err(e) => UpdateReport {
            name,
            status: UpdateStatus::Failed(format!("appimageupdatetool failed: {e}")),
        },
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
        // Oldest first by mtime (name sorting misorders "10.0.0" < "2.0.0").
        vers.sort_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        });
        if vers.len() >= 2 {
            // Find what `active` currently points at, go to the previous.
            let active = app_dir.join("active");
            let cur_target = std::fs::read_link(&active).ok();
            let cur_ver = cur_target
                .as_ref()
                .and_then(|t| t.parent())
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str());
            let idx = cur_ver.and_then(|v| {
                vers.iter()
                    .position(|p| p.file_name().and_then(|n| n.to_str()) == Some(v))
            });
            let prev_idx = match idx {
                Some(i) if i > 0 => i - 1,
                Some(_) => {
                    return Err(Error::NotInstalled(format!(
                        "{} is already at its oldest installed version",
                        inst.name
                    )))
                }
                None => vers.len() - 2, // can't tell what's active; assume latest
            };
            let prev_ver = &vers[prev_idx];
            // Find the AppImage inside the previous generation.
            let prev_img = std::fs::read_dir(prev_ver)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("appimage"))
                        .unwrap_or(false)
                });
            let Some(prev_img) = prev_img else {
                return Err(Error::NotInstalled(
                    "previous generation has no AppImage".to_string(),
                ));
            };
            crate::handlers::appimage::flip_symlink(&active, &prev_img)?;
            let ver_name = prev_ver
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("previous");
            ledger.clear_version(&slug).ok();
            return Ok(format!("rolled back to generation {ver_name}"));
        } else {
            // Only one generation installed: nothing to roll back to.
            return Err(Error::NotInstalled(format!(
                "no previous version of {} to roll back to",
                inst.name
            )));
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
        return Err(Error::NotInstalled(format!(
            "no snapshots for {}",
            inst.name
        )));
    }
    snaps.sort();
    let latest = snaps
        .pop()
        .ok_or_else(|| Error::NotInstalled(format!("no snapshots for {}", inst.name)))?;
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
    let file_name = local
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("appimage");
    let current_bak = snap_dir.join(format!("{file_name}-{nanos}.bak"));
    if local.exists() {
        std::fs::rename(local, &current_bak)?;
    }
    std::fs::rename(&latest, local).map_err(|e| {
        std::fs::rename(&current_bak, local).ok();
        Error::Io(std::io::Error::other(format!(
            "restoring snapshot: {e} (current file preserved)"
        )))
    })?;
    prune_snapshots(&snap_dir, 2);
    ledger.clear_version(&slug).ok();
    Ok(format!(
        "rolled back to {}",
        latest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("snapshot")
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

    let asset = match crate::github::pick_appimage_asset_for(&release, &inst.name) {
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
    let tmpdir = crate::unique_temp_dir("dropin-gh");
    let _tmp_guard = TempDirGuard(tmpdir.clone());
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

    // If the release ships a detached signature for this asset, fetch it too
    // so the continuity check below has something to verify.
    for ext in ["asc", "sig"] {
        let sig_name = format!("{}.{}", asset.name, ext);
        if let Some(sig_asset) = release.assets.iter().find(|a| a.name == sig_name) {
            let sig_dest = tmpdir.join(&sig_name);
            if crate::zsync::fetch::download_to_file(
                &sig_asset.download_url,
                &sig_dest,
                10 * 1024 * 1024,
            )
            .is_ok()
            {
                break;
            }
        }
    }

    // Signature continuity: if the new download is signed, its key must
    // match the pinned key (or pin it now, TOFU, if none was pinned).
    // If a key was pinned and the new release is unsigned or its signature
    // is unreadable, refuse: otherwise stripping the .asc would bypass the
    // continuity check entirely.
    match crate::signatures::check(&downloaded) {
        crate::signatures::SigCheck::Bad(why) => {
            return UpdateReport {
                name,
                status: UpdateStatus::Failed(format!(
                    "release signature is BAD ({why}) — refusing update"
                )),
            };
        }
        crate::signatures::SigCheck::Found {
            fingerprint,
            verified,
        } => {
            if verified {
                eprintln!("dropin: update signature verified with gpg");
            }
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
        crate::signatures::SigCheck::Invalid(why) => {
            if inst.signing_key.is_some() {
                return UpdateReport {
                    name,
                    status: UpdateStatus::Failed(format!(
                        "release signature is unreadable ({why}) but this app was previously signed — refusing update"
                    )),
                };
            }
        }
        crate::signatures::SigCheck::None => {
            if let Some(pinned) = &inst.signing_key {
                return UpdateReport {
                    name,
                    status: UpdateStatus::Failed(format!(
                        "release is unsigned, but this app was previously signed with key {pinned} — refusing update. Re-download manually if you trust this release."
                    )),
                };
            }
        }
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
                status: UpdateStatus::Failed(format!(
                    "could not migrate to generation layout: {e}"
                )),
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "unknown".to_string()
    } else {
        s
    }
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
    let ver = inst
        .version
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let ver = sanitize_version(&ver);
    let ver_dir = app_dir.join(&ver);
    std::fs::create_dir_all(&ver_dir)?;
    let file_name = cur_path
        .file_name()
        .ok_or_else(|| crate::Error::NotInstalled("bad file name".into()))?;
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

/// Keep only the newest `keep` generation dirs, oldest first by mtime.
/// (Name sorting misorders versions: "10.0.0" < "2.0.0" lexicographically.)
fn prune_generations(app_dir: &std::path::Path, keep: usize) {
    let mut vers: Vec<_> = std::fs::read_dir(app_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    vers.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    });
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
        // tar/unzip/ar are no longer needed: pure-Rust archive handling.
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
        {
            let (ok, why) = if !have("bwrap") {
                (
                    false,
                    "sandboxed installer scripts (required for .sh/.run/.bin)".to_string(),
                )
            } else {
                match handlers::script::bwrap_probe() {
                    Ok(()) => (
                        true,
                        "sandboxed installer scripts (sandbox works)".to_string(),
                    ),
                    Err(e) => (
                        false,
                        format!("bwrap is installed but can't create a sandbox: {e}"),
                    ),
                }
            };
            ("bwrap".into(), ok, why)
        },
        (
            "wine".into(),
            handlers::wine::have_wine(),
            ".exe installer support (optional)".into(),
        ),
        (
            "umu-run".into(),
            handlers::proton::have_umu(),
            "GE-Proton game support (optional)".into(),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_prefers_desktop_entry_matching_the_app() {
        let d = crate::unique_temp_dir("dropin-launch-desktop");
        let home = d.join("home");
        crate::test_env::with_home(&home, || {
            let dirs = UserDirs::new();
            dirs.ensure().unwrap();
            let xterm = dirs.desktop_entries.join("dropin-debian-xterm.desktop");
            let uxterm = dirs.desktop_entries.join("dropin-debian-uxterm.desktop");
            std::fs::write(
                &xterm,
                "[Desktop Entry]\nName=XTerm\nExec=/opt/xterm/usr/bin/xterm\n",
            )
            .unwrap();
            std::fs::write(
                &uxterm,
                "[Desktop Entry]\nName=UXTerm\nExec=/opt/xterm/usr/bin/uxterm\n",
            )
            .unwrap();
            let inst = crate::ledger::Install {
                id: 1,
                name: "xterm".into(),
                format: "Debian package (.deb)".into(),
                version: None,
                source_path: "/tmp/xterm.deb".into(),
                source_sha256: None,
                install_root: None,
                external: false,
                update_info: None,
                verified: false,
                github_repo: None,
                signing_key: None,
                env_vars: None,
                created_at: String::new(),
                files: vec![uxterm.display().to_string(), xterm.display().to_string()],
            };
            let found = find_managed_desktop(&inst, &dirs, "xterm").unwrap();
            assert_eq!(found, xterm, "picked the xterm entry, not uxterm");
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rollback_flips_active_to_previous_generation() {
        let d = crate::unique_temp_dir("dropin-rollback");
        let home = d.join("home");
        crate::test_env::with_home(&home, || {
            let dirs = UserDirs::new();
            dirs.ensure().unwrap();
            let app_dir = dirs.applications.join("demo");
            let v1 = app_dir.join("1.0");
            let v2 = app_dir.join("2.0");
            std::fs::create_dir_all(&v1).unwrap();
            std::fs::create_dir_all(&v2).unwrap();
            let img1 = v1.join("demo.AppImage");
            let img2 = v2.join("demo.AppImage");
            std::fs::write(&img1, b"v1").unwrap();
            std::fs::write(&img2, b"v2").unwrap();
            // Make v1 older so mtime ordering is deterministic.
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
            let _ = filetime_set(&v1, old);
            let _ = filetime_set(&img1, old);
            let _ = filetime_set(&v2, std::time::SystemTime::now());
            let _ = filetime_set(&img2, std::time::SystemTime::now());
            let active = app_dir.join("active");
            crate::handlers::appimage::flip_symlink(&active, &img2).unwrap();

            let ledger = Ledger::open().unwrap();
            ledger
                .record(&crate::ledger::NewInstall {
                    name: "demo".into(),
                    format: "AppImage".into(),
                    version: Some("2.0".into()),
                    source_path: PathBuf::from("/tmp/demo.AppImage"),
                    files: vec![app_dir.clone()],
                    ..Default::default()
                })
                .unwrap();

            assert!(can_rollback("demo"));
            let msg = rollback("demo").unwrap();
            assert!(msg.contains("1.0"), "rolled back to 1.0: {msg}");
            let target = std::fs::read_link(&active).unwrap();
            let abs = if target.is_absolute() {
                target
            } else {
                active.parent().unwrap().join(target)
            };
            assert_eq!(std::fs::read(&abs).unwrap(), b"v1");
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Set a file's mtime without pulling in a new dependency.
    fn filetime_set(path: &Path, t: std::time::SystemTime) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let times = [libc::timespec {
            tv_sec: secs,
            tv_nsec: 0,
        }; 2];
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[test]
    fn migrate_flat_to_generations() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("dropin-mig-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let home = d.join("home");
        crate::test_env::with_home(&home, || {
            let dirs = UserDirs::new();
            dirs.ensure().unwrap();
            // Fake legacy flat install.
            let flat = dirs.applications.join("OldApp.AppImage");
            std::fs::write(&flat, b"fake").unwrap();
            let desktop = dirs.desktop_entries.join("dropin-oldapp.desktop");
            std::fs::write(
                &desktop,
                "[Desktop Entry]\nName=OldApp\nExec=/old/path/OldApp.AppImage --flag\n",
            )
            .unwrap();
            let ledger = Ledger::open().unwrap();
            ledger
                .record(&crate::ledger::NewInstall {
                    name: "oldapp".into(),
                    format: "AppImage".into(),
                    version: Some("1.0".into()),
                    source_path: PathBuf::from("/tmp/OldApp.AppImage"),
                    files: vec![flat.clone(), desktop.clone()],
                    ..Default::default()
                })
                .unwrap();

            let inst = ledger.get("oldapp").unwrap();
            migrate_to_generations(&inst, &dirs).unwrap();

            // Flat file moved into version dir.
            assert!(!flat.exists(), "flat file gone");
            let ver_file = dirs.applications.join("oldapp/1.0/OldApp.AppImage");
            assert!(ver_file.exists(), "in version dir");
            // Active symlink points at it.
            let active = dirs.applications.join("oldapp/active");
            assert!(active.is_symlink());
            // Desktop Exec rewritten to active, args preserved.
            let text = std::fs::read_to_string(&desktop).unwrap();
            assert!(
                text.contains(&format!("Exec={} --flag", active.display())),
                "Exec rewritten: {text}"
            );
            // Ledger tracks the app dir now, not the flat file.
            let inst2 = ledger.get("oldapp").unwrap();
            assert!(
                inst2.files.iter().any(|f| f.ends_with("oldapp")),
                "app dir tracked: {:?}",
                inst2.files
            );
            assert!(
                !inst2.files.iter().any(|f| f.ends_with("OldApp.AppImage")),
                "flat file untracked"
            );
        });
        let _ = std::fs::remove_dir_all(&d);
    }
}
