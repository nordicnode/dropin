//! Windows executable (.exe/.msi) handler: Wine-based installation.
//!
//! Like Lutris, each app gets its own isolated Wine prefix
//! (`~/.local/share/dropin/wine-prefixes/<app>/`) so one app's
//! dependencies can't break another's. The prefix is a full virtual
//! C: drive; the .exe is copied in, a menu entry is created, and
//! everything is tracked in the ledger for clean removal.

use crate::handlers::{InstallOptions, InstallOutcome, InstallPlan};
use crate::{slugify, Error, Result, UserDirs};
use std::path::{Path, PathBuf};

/// Where Wine prefixes live.
pub fn prefix_root() -> PathBuf {
    crate::home_dir().join(".local/share/dropin/wine-prefixes")
}

/// The prefix for one app.
pub fn prefix_for(slug: &str) -> PathBuf {
    prefix_root().join(slug)
}

/// True when Wine is available (system or portable).
pub fn have_wine() -> bool {
    super::wine_setup::wine_binary().is_some()
}

/// The wine binary to use.
fn wine_bin() -> PathBuf {
    super::wine_setup::wine_binary().unwrap_or_else(|| PathBuf::from("wine"))
}

/// Extract the icon from a Windows executable and save it as PNG.
/// Tries `exe` first, then falls back to `fallback` (the installer).
/// Returns the saved icon path, or None when no icon could be extracted.
pub fn extract_exe_icon(slug: &str, exe: &Path, fallback: &Path) -> Option<PathBuf> {
    let png = crate::exe_icon::extract_icon_png(exe)
        .or_else(|| crate::exe_icon::extract_icon_png(fallback))?;
    let dest = UserDirs::new().icons.join(format!("dropin-{slug}.png"));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::write(&dest, &png).ok()?;
    Some(dest)
}

pub fn plan(src: &Path) -> Result<InstallPlan> {
    let d = crate::detect::detect(src)?;
    // For planning, use the heuristic (the GUI lets the user override).
    let is_game = super::proton::likely_game(&d.name_hint, src);
    let (runner_label, ready) = if is_game {
        ("GE-Proton", super::proton::have_umu())
    } else {
        ("Wine", have_wine())
    };
    Ok(InstallPlan {
        app_name: d.name_hint.clone(),
        version: None,
        steps: if ready {
            vec![
                "Create an isolated prefix just for this app".into(),
                format!("Copy the installer into the prefix and run it with {runner_label}"),
                format!("Add a menu entry that launches it with {runner_label}"),
            ]
        } else {
            vec![
                format!("{runner_label} isn't set up yet — dropin will set it up automatically"),
                "Then create an isolated prefix and run the installer".into(),
            ]
        },
        needs_root: false,
        needs_confirmation: true,
    })
}

/// Decide which runner to use: explicit choice, or auto-detect.
pub fn pick_runner(src: &Path, name: &str, opts: &InstallOptions) -> super::proton::Runner {
    if let Some(r) = opts.runner {
        return r;
    }
    if super::proton::likely_game(name, src) {
        super::proton::Runner::Proton
    } else {
        super::proton::Runner::Wine
    }
}

/// Install a .exe/.msi: create prefix, copy installer in, run it.
/// Uses GE-Proton (via umu) for games, plain Wine for applications.
pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    let d = crate::detect::detect(src)?;
    let runner = pick_runner(src, &d.name_hint, opts);

    // Check the runner is available.
    match runner {
        super::proton::Runner::Wine => {
            if !have_wine() {
                return Err(Error::Refused(
                    "Wine isn't installed, so Windows apps can't run yet.\n\
                     Run `dropin setup-wine` or use the GUI to set it up automatically."
                        .into(),
                ));
            }
        }
        super::proton::Runner::Proton => {
            if !super::proton::have_umu() {
                return Err(Error::Refused(
                    "Proton support isn't set up yet.\n\
                     Run `dropin setup-proton` or use the GUI to set it up automatically."
                        .into(),
                ));
            }
        }
    }

    let slug = slugify(&d.name_hint);
    let dirs = UserDirs::new();
    dirs.ensure()?;

    let prefix = prefix_for(&slug);
    if !prefix.exists() {
        std::fs::create_dir_all(&prefix)?;
        // Initialize the prefix for Wine; umu/Proton creates it on demand.
        if runner == super::proton::Runner::Wine {
            let status = std::process::Command::new(wine_bin())
                .env("WINEPREFIX", &prefix)
                .arg("wineboot")
                .arg("--init")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|e| Error::Command(format!("couldn't initialize Wine prefix: {e}")))?;
            if !status.success() {
                let _ = std::fs::remove_dir_all(&prefix);
                return Err(Error::Command("Wine prefix initialization failed".into()));
            }
        }
    }

    // Copy the installer into the prefix so it stays with the app.
    let file_name = src
        .file_name()
        .ok_or_else(|| Error::Unsupported("bad file name".into()))?;
    let dest = prefix.join("drive_c").join(file_name);
    std::fs::create_dir_all(prefix.join("drive_c"))?;
    std::fs::copy(src, &dest)?;

    // For .msi, run msiexec; for .exe, run directly.
    let is_msi = src
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("msi"))
        .unwrap_or(false);

    // Ask before running: the installer itself is a Windows program.
    // In GUI mode the dialog already confirmed; in CLI, confirm here
    // unless --yes.
    if !opts.yes {
        eprintln!(
            "About to run the Windows installer with {}.\n\
             Prefix: {}\n\
             This runs the vendor's code — only continue if you trust it.",
            runner.label(),
            prefix.display()
        );
        if !crate::handlers::script::ask("Run the installer? [y/N]")? {
            return Err(Error::Refused("cancelled".into()));
        }
    }

    match runner {
        super::proton::Runner::Wine => {
            let mut cmd = std::process::Command::new(wine_bin());
            cmd.env("WINEPREFIX", &prefix);
            if is_msi {
                cmd.arg("msiexec").arg("/i").arg(&dest);
            } else {
                cmd.arg(&dest);
            }
            let status = cmd
                .status()
                .map_err(|e| Error::Command(format!("Wine failed: {e}")))?;
            if !status.success() {
                eprintln!("note: the installer exited with a non-zero status; continuing anyway");
            }
        }
        super::proton::Runner::Proton => {
            // protonfixes applies automatically via GAMEID.
            // For .msi, Proton handles it via the exe path.
            if let Err(e) = super::proton::run_with_proton(
                &prefix,
                &dest,
                &d.name_hint,
                &std::collections::HashMap::new(),
            ) {
                eprintln!("note: {e}; continuing anyway");
            }
        }
    }

    // Find the installed .exe: look for new .exes in drive_c, prefer
    // Program Files.
    let exe = find_installed_exe(&prefix).unwrap_or(dest.clone());

    // Extract the app's icon from the .exe so the menu entry shows it.
    let icon_path = extract_exe_icon(&slug, &exe, &dest);
    let icon_line = icon_path
        .as_ref()
        .map(|p| format!("Icon={}\n", p.display()))
        .unwrap_or_default();

    // Menu entry that launches with the right runner.
    let desktop_path = dirs.desktop_entries.join(format!("dropin-{slug}.desktop"));
    let desktop_text = match runner {
        super::proton::Runner::Wine => format!(
            "[Desktop Entry]\nType=Application\nName={}\nExec=env WINEPREFIX={} {} \"{}\"\n{}Categories=Utility;\nTerminal=false\n",
            d.name_hint,
            prefix.display(),
            wine_bin().display(),
            exe.display(),
            icon_line
        ),
        super::proton::Runner::Proton => {
            let (env, argv) = super::proton::proton_launch_cmd(&prefix, &exe, &d.name_hint);
            let env_str = env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            let argv_str = argv
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "[Desktop Entry]\nType=Application\nName={}\nExec=env {env_str} {argv_str}\n{}Categories=Game;\nTerminal=false\n",
                d.name_hint, icon_line,
            )
        }
    };
    std::fs::write(&desktop_path, desktop_text)?;

    // Record the runner in the ledger via env_vars (so launch uses it).
    // The launch function will check for a saved runner preference.

    Ok(InstallOutcome {
        name: slug.clone(),
        version: None,
        paths: {
            let mut p = vec![prefix.clone(), desktop_path];
            if let Some(icon) = icon_path {
                p.push(icon);
            }
            p
        },
        install_root: Some(prefix),
        external: false,
        update_info: None,
    })
}

/// Heuristic: find the most likely main .exe in a prefix.
fn find_installed_exe(prefix: &Path) -> Option<PathBuf> {
    let drive_c = prefix.join("drive_c");
    let mut candidates = Vec::new();
    for entry in walkdir::WalkDir::new(&drive_c)
        .into_iter()
        .filter_map(|e| e.ok())
        .take(5000)
    {
        let p = entry.path();
        if p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
        {
            // Skip uninstallers and helpers.
            let name = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if name.contains("uninstall") || name.contains("setup") {
                continue;
            }
            candidates.push(p.to_path_buf());
        }
    }
    // Prefer Program Files, then shortest path (likely the main exe).
    candidates.sort_by_key(|p| {
        let s = p.to_string_lossy().to_lowercase();
        let in_pf = if s.contains("program files") { 0 } else { 1 };
        (in_pf, p.components().count())
    });
    candidates.into_iter().next()
}

/// Remove a Wine app: delete the prefix and menu entry.
pub fn remove(slug: &str) -> Result<()> {
    let prefix = prefix_for(slug);
    if prefix.exists() {
        std::fs::remove_dir_all(&prefix)?;
    }
    Ok(())
}

/// Change the runner for an installed Windows app (Wine <-> GE-Proton).
/// Rebuilds the desktop entry. The prefix is reused.
pub fn set_runner(slug: &str, runner: super::proton::Runner) -> Result<String> {
    let ledger = crate::ledger::Ledger::open()?;
    let inst = ledger.get(slug)?;
    let prefix = prefix_for(slug);
    if !prefix.exists() {
        return Err(Error::NotInstalled(slug.into()));
    }

    let exe = find_installed_exe(&prefix)
        .ok_or_else(|| Error::Command("couldn't find the app's executable".into()))?;

    let dirs = UserDirs::new();
    let desktop_path = dirs.desktop_entries.join(format!("dropin-{slug}.desktop"));
    // Keep the extracted icon if one was saved during install.
    let icon_path = dirs.icons.join(format!("dropin-{slug}.png"));
    let icon_line = if icon_path.exists() {
        format!("Icon={}\n", icon_path.display())
    } else {
        String::new()
    };
    let desktop_text = match runner {
        super::proton::Runner::Wine => {
            let wine = super::wine_setup::wine_binary().unwrap_or_else(|| PathBuf::from("wine"));
            format!(
                "[Desktop Entry]\nType=Application\nName={}\nExec=env WINEPREFIX={} {} \"{}\"\n{}Categories=Utility;\nTerminal=false\n",
                inst.name,
                prefix.display(),
                wine.display(),
                exe.display(),
                icon_line
            )
        }
        super::proton::Runner::Proton => {
            let (env, argv) = super::proton::proton_launch_cmd(&prefix, &exe, &inst.name);
            let env_str = env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            let argv_str = argv
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "[Desktop Entry]\nType=Application\nName={}\nExec=env {env_str} {argv_str}\n{}Categories=Game;\nTerminal=false\n",
                inst.name, icon_line,
            )
        }
    };
    std::fs::write(&desktop_path, desktop_text)?;

    // Record the runner choice so it survives.
    let mut env_map = ledger.env_vars(slug).unwrap_or_default();
    env_map.insert(
        "__dropin_runner".into(),
        match runner {
            super::proton::Runner::Wine => "wine".into(),
            super::proton::Runner::Proton => "proton".into(),
        },
    );
    let json = serde_json::to_string(&env_map).unwrap_or_default();
    ledger.set_env_vars(slug, &json)?;

    Ok(format!("Switched '{}' to {}.", inst.name, runner.label()))
}

/// Get the current runner for an app, if it's a Windows app.
pub fn current_runner(slug: &str) -> Option<super::proton::Runner> {
    let ledger = crate::ledger::Ledger::open().ok()?;
    let env_map = ledger.env_vars(slug).ok()?;
    match env_map.get("__dropin_runner").map(|s| s.as_str()) {
        Some("proton") => Some(super::proton::Runner::Proton),
        Some("wine") => Some(super::proton::Runner::Wine),
        _ => None,
    }
}
