//! Adopt an already-installed game or app (Lutris-style "add existing game").
//!
//! The user points at a directory containing the game. We scan for the main
//! executable, detect Windows vs Linux, set up Wine/Proton if needed, and
//! register everything in the ledger — tracking files in place, not moving them.

use crate::desktop::{generate_entry, parse_desktop_file};
use crate::{slugify, Error, Result, UserDirs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// What we found when scanning a directory.
#[derive(Debug, Clone)]
pub struct AdoptScan {
    /// All executables found (relative to the scanned dir).
    pub executables: Vec<String>,
    /// .desktop files found (relative paths).
    pub desktop_files: Vec<String>,
    /// Whether any .exe files were found (suggests Windows game).
    pub has_exe: bool,
    /// Whether any ELF executables were found (suggests Linux game).
    pub has_elf: bool,
}

/// Scan a directory for executables and desktop files.
pub fn scan_dir(dir: &Path) -> Result<AdoptScan> {
    if !dir.is_dir() {
        return Err(Error::Unsupported(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let mut executables = Vec::new();
    let mut desktop_files = Vec::new();
    let mut has_exe = false;
    let mut has_elf = false;

    for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        if rel.is_empty() {
            continue;
        }

        // Check for .desktop files.
        if path.extension().map(|e| e == "desktop").unwrap_or(false) {
            desktop_files.push(rel.clone());
        }

        // Check for executables.
        if entry.file_type().is_file() {
            if is_windows_exe(path) {
                has_exe = true;
                executables.push(rel);
            } else if is_elf_executable(path) {
                has_elf = true;
                executables.push(rel);
            }
        }
    }

    // Sort: prefer shallow paths and name matches (done by caller with context).
    executables.sort_by_key(|p| (p.matches('/').count(), p.len()));
    desktop_files.sort();

    Ok(AdoptScan {
        executables,
        desktop_files,
        has_exe,
        has_elf,
    })
}

fn is_windows_exe(path: &Path) -> bool {
    // Check extension first (fast path).
    if path
        .extension()
        .map(|e| e == "exe" || e == "EXE")
        .unwrap_or(false)
    {
        return true;
    }
    // Check MZ magic.
    let mut buf = [0u8; 2];
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open(path) {
        if f.read_exact(&mut buf).is_ok() && buf == *b"MZ" {
            return true;
        }
    }
    false
}

fn is_elf_executable(path: &Path) -> bool {
    let mut buf = [0u8; 4];
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    if f.read_exact(&mut buf).is_err() {
        return false;
    }
    if buf == [0x7f, b'E', b'L', b'F'] {
        return true;
    }
    // Script with shebang and exec bit.
    if !(buf[0] == b'#' && buf[1] == b'!') {
        return false;
    }
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Options for adopting a game.
#[derive(Debug, Clone)]
pub struct AdoptOptions {
    /// The main executable (relative to game_dir). If None, we pick the best guess.
    pub main_exe: Option<String>,
    /// For Windows games: true if it's a game (Proton), false if app (Wine).
    /// If None, we guess from the name.
    pub is_game: Option<bool>,
    /// Custom display name. If None, derived from directory name.
    pub name: Option<String>,
}

/// Adopt an already-installed game directory.
/// Tracks files in place (does not move them).
pub fn adopt(game_dir: &Path, opts: &AdoptOptions) -> Result<AdoptOutcome> {
    let game_dir = game_dir.canonicalize().map_err(|e| {
        Error::Command(format!(
            "Couldn't access {}.\n\nDetails: {e}",
            game_dir.display()
        ))
    })?;

    let scan = scan_dir(&game_dir)?;
    if scan.executables.is_empty() && scan.desktop_files.is_empty() {
        return Err(Error::Refused(format!(
            "No executables found in {}. This doesn't look like a game.",
            game_dir.display()
        )));
    }

    // Determine the main executable.
    let main_exe_rel = match &opts.main_exe {
        Some(e) => e.clone(),
        None => choose_main_exe(&scan, &game_dir)?,
    };
    let main_exe = game_dir.join(&main_exe_rel);
    if !main_exe.exists() {
        return Err(Error::Command(format!(
            "Main executable not found: {}",
            main_exe.display()
        )));
    }

    // Detect Windows vs Linux.
    let is_windows = is_windows_exe(&main_exe);
    let name = opts.name.clone().unwrap_or_else(|| {
        game_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "game".to_string())
    });
    let slug = slugify(&name);

    let dirs = UserDirs::new();
    dirs.ensure()?;

    // Check not already tracked.
    let ledger = crate::ledger::Ledger::open()?;
    if ledger.get(&slug).is_ok() {
        return Err(Error::Refused(format!(
            "{slug} is already tracked — remove it first with `dropin remove {slug}`"
        )));
    }

    // Collect all files for the ledger (track in place).
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&game_dir)
        .into_iter()
        .flatten()
        .map(|e| e.path().to_path_buf())
        .collect();

    // For Windows games: set up Wine/Proton prefix.
    let mut extra_paths = Vec::new();
    let launch_exe: PathBuf;
    if is_windows {
        let is_game = opts
            .is_game
            .unwrap_or_else(|| crate::handlers::proton::likely_game(&name, &main_exe));
        let prefix = dirs.data.join("wine-prefixes").join(&slug);
        std::fs::create_dir_all(&prefix)?;

        if is_game {
            // Set up Proton.
            if !crate::handlers::proton::have_umu() {
                return Err(Error::Command(
                    "Proton isn't set up yet. Install a game via dropin first, or run `dropin setup-proton`.".into(),
                ));
            }
            // The exe stays in place; Proton runs it from there with the prefix.
            launch_exe = main_exe.clone();
            // Record the prefix for removal.
            extra_paths.push(prefix);
        } else {
            // Set up Wine.
            if !crate::handlers::wine::have_wine() {
                return Err(Error::Command(
                    "Wine isn't installed. Install a Windows app via dropin first.".into(),
                ));
            }
            launch_exe = main_exe.clone();
            extra_paths.push(prefix);
        }
        paths.extend(extra_paths.clone());
    } else {
        launch_exe = main_exe.clone();
    }

    // Create .desktop entry.
    let (desktop_path, mut desktop_extra): (PathBuf, Vec<PathBuf>) = if is_windows {
        let is_game = opts
            .is_game
            .unwrap_or_else(|| crate::handlers::proton::likely_game(&name, &main_exe));
        let prefix = dirs.data.join("wine-prefixes").join(&slug);
        if is_game {
            let (env, argv) =
                crate::handlers::proton::proton_launch_cmd(&prefix, &launch_exe, &name);
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
            let entry = crate::desktop::DesktopEntry {
                name: name.clone(),
                exec: format!("env {env_str} {argv_str}"),
                icon: None,
                comment: None,
                categories: Some("Game;".into()),
                terminal: false,
            };
            (entry.write(&slug)?, vec![])
        } else {
            let entry = crate::desktop::DesktopEntry {
                name: name.clone(),
                exec: format!(
                    "env WINEPREFIX=\"{}\" wine \"{}\"",
                    prefix.display(),
                    launch_exe.display()
                ),
                icon: None,
                comment: None,
                categories: None,
                terminal: false,
            };
            (entry.write(&slug)?, vec![])
        }
    } else {
        // Native Linux: adopt .desktop if present, else generate.
        if let Some(df_rel) = scan.desktop_files.first() {
            let df_path = game_dir.join(df_rel);
            if parse_desktop_file(&df_path).is_ok() {
                crate::desktop::adopt_desktop_file(&df_path, &slug, &name, &game_dir)?
            } else {
                generate_entry(&name, &slug, &launch_exe)?
            }
        } else {
            generate_entry(&name, &slug, &launch_exe)?
        }
    };
    paths.push(desktop_path);
    paths.append(&mut desktop_extra);

    // Symlink into ~/.local/bin.
    if let Some(bin_name) = launch_exe.file_name() {
        let link = dirs.bin.join(bin_name);
        if !link.exists() && !link.is_symlink() {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&launch_exe, &link)?;
                paths.push(link);
            }
        }
    }

    // Record in ledger.
    let new_install = crate::ledger::NewInstall {
        name: slug.clone(),
        format: if is_windows {
            "Windows executable".into()
        } else {
            "adopted".into()
        },
        version: None,
        source_path: game_dir.clone(),
        source_sha256: None,
        install_root: Some(game_dir.clone()),
        external: true, // Tracked in place, not managed by dropin
        update_info: None,
        verified: false,
        github_repo: None,
        signing_key: None,
        env_vars: None,
        files: paths,
    };
    ledger.record(&new_install)?;

    Ok(AdoptOutcome {
        name: slug,
        game_dir,
        main_exe: launch_exe,
        is_windows,
    })
}

/// Choose the best main executable from scan results.
fn choose_main_exe(scan: &AdoptScan, game_dir: &Path) -> Result<String> {
    if scan.executables.is_empty() {
        return Err(Error::Refused("No executables found.".into()));
    }
    if scan.executables.len() == 1 {
        return Ok(scan.executables[0].clone());
    }
    // Prefer executables whose name matches the directory name.
    let dir_name = game_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut cands = scan.executables.clone();
    cands.sort_by_key(|p| {
        let stem = Path::new(p)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let name_match = stem == dir_name || dir_name.contains(&stem) || stem.contains(&dir_name);
        // Prefer .exe for Windows, ELF for Linux; avoid uninstallers.
        let is_uninstaller = stem.contains("uninstall") || stem.contains("unins");
        let depth = p.matches('/').count();
        (!name_match, is_uninstaller, depth, p.len())
    });
    Ok(cands.into_iter().next().unwrap())
}

/// Result of adopting a game.
#[derive(Debug)]
pub struct AdoptOutcome {
    pub name: String,
    pub game_dir: PathBuf,
    pub main_exe: PathBuf,
    pub is_windows: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::with_home;

    #[test]
    fn scan_finds_executables() {
        let d = std::env::temp_dir().join(format!("dropin-adopt-test-{}", std::process::id()));
        let home = d.join("home");
        crate::test_env::with_home(&home, || {
            let dir = home.join("testgame");
            std::fs::create_dir_all(&dir).unwrap();
            // Fake ELF.
            std::fs::write(dir.join("game"), [0x7f, b'E', b'L', b'F', 0, 0, 0, 0]).unwrap();
            // Fake text file (not executable).
            std::fs::write(dir.join("readme.txt"), b"hello").unwrap();

            let scan = scan_dir(&dir).unwrap();
            assert_eq!(scan.executables.len(), 1);
            assert!(scan.has_elf);
            assert!(!scan.has_exe);
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scan_finds_windows_exe() {
        let d = std::env::temp_dir().join(format!("dropin-adopt-test-{}", std::process::id()));
        let home = d.join("home");
        crate::test_env::with_home(&home, || {
            let dir = home.join("wingame");
            std::fs::create_dir_all(&dir).unwrap();
            // Fake MZ.
            std::fs::write(dir.join("game.exe"), [b'M', b'Z', 0, 0]).unwrap();

            let scan = scan_dir(&dir).unwrap();
            assert_eq!(scan.executables.len(), 1);
            assert!(scan.has_exe);
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scan_rejects_non_directory() {
        let d = std::env::temp_dir().join(format!("dropin-adopt-test-{}", std::process::id()));
        let home = d.join("home");
        crate::test_env::with_home(&home, || {
            let file = home.join("notadir.txt");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(&file, b"hi").unwrap();
            assert!(scan_dir(&file).is_err());
        });
        let _ = std::fs::remove_dir_all(&d);
    }
}
