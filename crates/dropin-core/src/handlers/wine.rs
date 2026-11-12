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
            let setup = if is_game {
                "dropin setup-proton"
            } else {
                "dropin setup-wine"
            };
            vec![
                format!("{runner_label} isn't set up yet — run `{setup}` first"),
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
                     Run `dropin setup-wine` to set it up automatically."
                        .into(),
                ));
            }
        }
        super::proton::Runner::Proton => {
            if !super::proton::have_umu() {
                return Err(Error::Refused(
                    "Proton support isn't set up yet.\n\
                     Run `dropin setup-proton` to set it up automatically."
                        .into(),
                ));
            }
        }
    }

    let slug = slugify(&d.name_hint);
    super::ensure_not_installed(&slug)?;
    let dirs = UserDirs::new();
    dirs.ensure()?;

    let prefix = prefix_for(&slug);
    if !prefix.exists() {
        std::fs::create_dir_all(&prefix)?;
        // Initialize the prefix for Wine; umu/Proton creates it on demand.
        if runner == super::proton::Runner::Wine {
            let mut init = std::process::Command::new(wine_bin());
            init.env("WINEPREFIX", &prefix)
                .arg("wineboot")
                .arg("--init")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let status = super::run_with_timeout(&mut init, std::time::Duration::from_secs(5 * 60))
                .map_err(|e| Error::Command(format!("couldn't initialize Wine prefix: {e}")))?;
            if !status.success() {
                let _ = std::fs::remove_dir_all(&prefix);
                return Err(Error::Command("Wine prefix initialization failed".into()));
            }
        }
    }

    // If anything below fails, remove the half-created prefix.
    let mut guard = super::RootGuard::new(prefix.clone());

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
            let status = super::run_with_timeout(&mut cmd, super::install_timeout())
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

    guard.disarm();
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
    find_main_exe(&prefix.join("drive_c"))
}

/// Find the most likely main .exe under an arbitrary directory.
/// Used for Wine prefixes (`drive_c`) and for adopted game folders.
pub fn find_main_exe(dir: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
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
        .or_else(|| {
            // Adopted Windows games keep their .exe in the user's folder,
            // not inside the Wine prefix.
            inst.install_root
                .as_deref()
                .and_then(|root| find_main_exe(std::path::Path::new(root)))
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_main_exe_prefers_program_files_and_skips_helpers() {
        let d = crate::unique_temp_dir("dropin-wine-exe");
        let drive = d.join("drive_c");
        std::fs::create_dir_all(drive.join("Program Files/Game")).unwrap();
        std::fs::create_dir_all(drive.join("windows")).unwrap();
        std::fs::write(drive.join("Program Files/Game/game.exe"), b"MZ").unwrap();
        std::fs::write(drive.join("Program Files/Game/uninstall.exe"), b"MZ").unwrap();
        std::fs::write(drive.join("Program Files/Game/setup.exe"), b"MZ").unwrap();
        std::fs::write(drive.join("windows/helper.exe"), b"MZ").unwrap();
        let found = find_main_exe(&drive).unwrap();
        assert_eq!(found, drive.join("Program Files/Game/game.exe"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn find_main_exe_returns_none_without_exes() {
        let d = crate::unique_temp_dir("dropin-wine-noexe");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("readme.txt"), b"hi").unwrap();
        assert!(find_main_exe(&d).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pick_runner_respects_explicit_choice() {
        let opts = InstallOptions {
            runner: Some(super::super::proton::Runner::Proton),
            ..Default::default()
        };
        assert_eq!(
            pick_runner(Path::new("/tmp/notepad.exe"), "notepad", &opts),
            super::super::proton::Runner::Proton
        );
        let opts = InstallOptions {
            runner: Some(super::super::proton::Runner::Wine),
            ..Default::default()
        };
        assert_eq!(
            pick_runner(Path::new("/tmp/setup_game.exe"), "setup_game", &opts),
            super::super::proton::Runner::Wine
        );
    }

    fn write_mock(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
    }

    #[test]
    fn install_launch_remove_with_mock_wine() {
        let d = crate::unique_temp_dir("dropin-wine-e2e");
        let home = d.join("home");
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_mock(
            &bin,
            "wine",
            "#!/bin/sh\n\
             if [ \"$1\" = wineboot ]; then\n\
               mkdir -p \"$WINEPREFIX/drive_c/Program Files/MockApp\"\n\
               exit 0\n\
             fi\n\
             mkdir -p \"$WINEPREFIX/drive_c/Program Files/MockApp\"\n\
             printf MZ > \"$WINEPREFIX/drive_c/Program Files/MockApp/mockapp.exe\"\n\
             exit 0\n",
        );
        let exe = d.join("fakeapp.exe");
        std::fs::write(&exe, b"MZ\x00\x00").unwrap();

        crate::test_env::with_home_and_path(&home, &bin, || {
            let opts = InstallOptions {
                yes: true,
                runner: Some(super::super::proton::Runner::Wine),
                ..Default::default()
            };
            let outcome = crate::install::install(&exe, &opts).unwrap();
            assert_eq!(outcome.name, "fakeapp");
            let prefix = prefix_for("fakeapp");
            assert!(prefix
                .join("drive_c/Program Files/MockApp/mockapp.exe")
                .exists());
            let desktop = home.join(".local/share/applications/dropin-fakeapp.desktop");
            assert!(desktop.exists(), "desktop entry created");
            let text = std::fs::read_to_string(&desktop).unwrap();
            assert!(
                text.contains("mockapp.exe"),
                "launches the installed exe: {text}"
            );

            // The ledger knows about it.
            let ledger = crate::ledger::Ledger::open().unwrap();
            assert!(ledger.get("fakeapp").is_ok());

            // launch resolves the desktop entry and runs the mock.
            let msg = crate::install::launch("fakeapp").unwrap();
            assert!(msg.contains("Launched"), "{msg}");

            // remove deletes the prefix and the menu entry.
            crate::install::remove("fakeapp", false).unwrap();
            assert!(!prefix.exists(), "prefix removed");
            assert!(!desktop.exists(), "desktop removed");
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn install_launch_remove_with_mock_proton() {
        let d = crate::unique_temp_dir("dropin-proton-e2e");
        let home = d.join("home");
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_mock(
            &bin,
            "umu-run",
            "#!/bin/sh\n\
             mkdir -p \"$WINEPREFIX/drive_c/Program Files/MockGame\"\n\
             printf MZ > \"$WINEPREFIX/drive_c/Program Files/MockGame/mockgame.exe\"\n\
             exit 0\n",
        );
        let exe = d.join("mockgame.exe");
        std::fs::write(&exe, b"MZ\x00\x00").unwrap();

        crate::test_env::with_home_and_path(&home, &bin, || {
            // Pre-populate the GAMEID cache so the test never hits the network.
            let umu = home.join(".local/share/dropin/umu");
            std::fs::create_dir_all(&umu).unwrap();
            std::fs::write(umu.join("gameids.json"), r#"{"mockgame":"umu-12345"}"#).unwrap();

            let opts = InstallOptions {
                yes: true,
                runner: Some(super::super::proton::Runner::Proton),
                ..Default::default()
            };
            let outcome = crate::install::install(&exe, &opts).unwrap();
            assert_eq!(outcome.name, "mockgame");
            let prefix = prefix_for("mockgame");
            assert!(prefix
                .join("drive_c/Program Files/MockGame/mockgame.exe")
                .exists());
            let desktop = home.join(".local/share/applications/dropin-mockgame.desktop");
            let text = std::fs::read_to_string(&desktop).unwrap();
            assert!(text.contains("umu-run"), "launches via umu-run: {text}");
            assert!(
                text.contains("GAMEID=umu-12345"),
                "uses the cached GAMEID: {text}"
            );

            crate::install::remove("mockgame", false).unwrap();
            assert!(!prefix.exists(), "prefix removed");
            assert!(!desktop.exists(), "desktop removed");
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn set_runner_rewrites_the_desktop_entry() {
        let d = crate::unique_temp_dir("dropin-setrunner-e2e");
        let home = d.join("home");
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_mock(
            &bin,
            "wine",
            "#!/bin/sh\n\
             if [ \"$1\" = wineboot ]; then\n\
               mkdir -p \"$WINEPREFIX/drive_c/Program Files/MockApp\"\n\
               exit 0\n\
             fi\n\
             mkdir -p \"$WINEPREFIX/drive_c/Program Files/MockApp\"\n\
             printf MZ > \"$WINEPREFIX/drive_c/Program Files/MockApp/mockapp.exe\"\n\
             exit 0\n",
        );
        write_mock(&bin, "umu-run", "#!/bin/sh\nexit 0\n");
        let exe = d.join("fakeapp.exe");
        std::fs::write(&exe, b"MZ\x00\x00").unwrap();

        crate::test_env::with_home_and_path(&home, &bin, || {
            let umu = home.join(".local/share/dropin/umu");
            std::fs::create_dir_all(&umu).unwrap();
            std::fs::write(umu.join("gameids.json"), r#"{"fakeapp":"umu-1"}"#).unwrap();

            let opts = InstallOptions {
                yes: true,
                runner: Some(super::super::proton::Runner::Wine),
                ..Default::default()
            };
            crate::install::install(&exe, &opts).unwrap();
            let desktop = home.join(".local/share/applications/dropin-fakeapp.desktop");
            assert!(std::fs::read_to_string(&desktop).unwrap().contains("wine"));

            set_runner("fakeapp", super::super::proton::Runner::Proton).unwrap();
            let text = std::fs::read_to_string(&desktop).unwrap();
            assert!(text.contains("umu-run"), "switched to Proton: {text}");
            assert_eq!(
                current_runner("fakeapp"),
                Some(super::super::proton::Runner::Proton)
            );

            set_runner("fakeapp", super::super::proton::Runner::Wine).unwrap();
            let text = std::fs::read_to_string(&desktop).unwrap();
            assert!(text.contains("wine"), "switched back to Wine: {text}");
            assert_eq!(
                current_runner("fakeapp"),
                Some(super::super::proton::Runner::Wine)
            );

            crate::install::remove("fakeapp", false).unwrap();
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn installs_a_real_pe_with_mock_wine() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello.exe");
        if !fixture.exists() {
            return;
        }
        let d = crate::unique_temp_dir("dropin-real-pe");
        let home = d.join("home");
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_mock(
            &bin,
            "wine",
            "#!/bin/sh\n\
             if [ \"$1\" = wineboot ]; then\n\
               mkdir -p \"$WINEPREFIX/drive_c/Program Files/Hello\"\n\
               exit 0\n\
             fi\n\
             mkdir -p \"$WINEPREFIX/drive_c/Program Files/Hello\"\n\
             cp \"$1\" \"$WINEPREFIX/drive_c/Program Files/Hello/hello.exe\"\n\
             exit 0\n",
        );

        crate::test_env::with_home_and_path(&home, &bin, || {
            // The real PE is detected by magic, not just extension.
            let det = crate::detect::detect(&fixture).unwrap();
            assert_eq!(det.format, crate::detect::Format::WindowsExe);

            let opts = InstallOptions {
                yes: true,
                runner: Some(super::super::proton::Runner::Wine),
                ..Default::default()
            };
            let outcome = crate::install::install(&fixture, &opts).unwrap();
            assert_eq!(outcome.name, "hello");
            let prefix = prefix_for("hello");
            assert!(prefix
                .join("drive_c/Program Files/Hello/hello.exe")
                .exists());
            let desktop = home.join(".local/share/applications/dropin-hello.desktop");
            assert!(desktop.exists());
            crate::install::remove("hello", false).unwrap();
            assert!(!prefix.exists());
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn installs_a_real_pe_with_real_wine() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello.exe");
        if !fixture.exists() {
            return;
        }
        let d = crate::unique_temp_dir("dropin-real-wine");
        let home = d.join("home");
        // Keep Wine quiet and stop it from trying to fetch Mono/Gecko.
        std::env::set_var("WINEDEBUG", "-all");
        std::env::set_var("WINEDLLOVERRIDES", "mscoree,mshtml=");
        crate::test_env::with_home(&home, || {
            // Check inside the same HOME the install will use: a Wine found
            // only in the real HOME isn't usable in the isolated test home.
            if !have_wine() {
                return;
            }
            let opts = InstallOptions {
                yes: true,
                runner: Some(super::super::proton::Runner::Wine),
                ..Default::default()
            };
            let outcome = crate::install::install(&fixture, &opts).unwrap();
            assert_eq!(outcome.name, "hello");
            let prefix = prefix_for("hello");
            assert!(prefix.join("drive_c").exists(), "Wine prefix created");
            let desktop = home.join(".local/share/applications/dropin-hello.desktop");
            assert!(desktop.exists(), "desktop entry created");
            crate::install::remove("hello", false).unwrap();
            assert!(!prefix.exists(), "prefix removed");
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pick_runner_auto_detects_games() {
        let opts = InstallOptions::default();
        assert_eq!(
            pick_runner(
                Path::new("/tmp/setup_witcher3.exe"),
                "setup_witcher3",
                &opts
            ),
            super::super::proton::Runner::Proton
        );
        assert_eq!(
            pick_runner(Path::new("/tmp/notepad.exe"), "notepad", &opts),
            super::super::proton::Runner::Wine
        );
    }
}
