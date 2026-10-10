//! Health checks: duplicates, shadowing, cross-source installs, orphans.
//!
//! Everything here is read-only. `dropin doctor` reports; it never changes
//! anything.

use crate::ledger::{Install, Ledger};
use crate::{have, UserDirs};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthKind {
    /// The ledger entry's files are gone (user deleted them by hand).
    Orphaned,
    /// Two dropin installs look like the same app.
    Duplicate,
    /// A dropin menu entry shadows (hides) a system one of the same name.
    Shadowed,
    /// The same app is also installed via a system package manager.
    CrossSource,
    /// A menu entry's Exec points at something that doesn't exist.
    BrokenExec,
    /// FUSE isn't available, so AppImages can't run.
    NoFuse,
}

/// True when the system can run AppImages (FUSE device or fusermount present).
pub fn have_fuse() -> bool {
    std::path::Path::new("/dev/fuse").exists()
        || crate::have("fusermount")
        || crate::have("fusermount3")
}

#[derive(Debug, Clone)]
pub struct HealthIssue {
    pub kind: HealthKind,
    pub message: String,
    pub hint: String,
}

/// Run all health checks. Best-effort: a check that can't run on this
/// system is skipped, never fatal.
pub fn check() -> Vec<HealthIssue> {
    let ledger = match Ledger::open() {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    let installs = ledger.list().unwrap_or_default();
    if installs.is_empty() {
        return Vec::new();
    }
    let dirs = UserDirs::new();
    check_with(&dirs, &system_desktop_dirs(), &installs)
}

fn check_with(dirs: &UserDirs, sys_desktop_dirs: &[PathBuf], installs: &[Install]) -> Vec<HealthIssue> {
    let mut issues = Vec::new();
    issues.extend(check_orphaned(installs));
    issues.extend(check_duplicates(dirs, installs));
    issues.extend(check_shadowed(dirs, sys_desktop_dirs, installs));
    issues.extend(check_cross_source(dirs, installs));
    issues.extend(check_broken_exec(dirs, installs));
    issues.extend(check_fuse(installs));
    issues
}

/// AppImages need FUSE to run. If the user has AppImages installed but no
/// FUSE, they won't launch — flag it with a distro-agnostic hint.
fn check_fuse(installs: &[Install]) -> Vec<HealthIssue> {
    let has_appimage = installs.iter().any(|i| i.format == "AppImage");
    if !has_appimage || have_fuse() {
        return Vec::new();
    }
    vec![HealthIssue {
        kind: HealthKind::NoFuse,
        message: "FUSE isn't available, so installed AppImages can't run".into(),
        hint: "install FUSE for your distro (e.g. `apt install fuse libfuse2`, `dnf install fuse`, `pacman -S fuse2`) or use --appimage-extract".into(),
    }]
}

/// Menu entries whose Exec binary doesn't exist. Catches the old dropin bug
/// that wrote `<AppImage>/AppRun` (joining AppRun onto the image file path
/// instead of using the image itself): the fix is to point Exec at the
/// AppImage, which reinstalling now does correctly.
fn check_broken_exec(dirs: &UserDirs, installs: &[Install]) -> Vec<HealthIssue> {
    let mut issues = Vec::new();
    for inst in installs {
        if inst.format != "AppImage" {
            continue;
        }
        // Menu entries are written as dropin-<slug>.desktop.
        let desktop = dirs
            .desktop_entries
            .join(format!("dropin-{}.desktop", inst.name));
        let text = match std::fs::read_to_string(&desktop) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let exec = text
            .lines()
            .find_map(|l| l.strip_prefix("Exec="))
            .map(str::trim)
            .unwrap_or("");
        let bin = exec.split_whitespace().next().unwrap_or("");
        if bin.is_empty() || Path::new(bin).exists() {
            continue;
        }
        // The known-bad pattern: <image>/AppRun where <image> is a file.
        let hint = if let Some(img) = bin.strip_suffix("/AppRun") {
            if Path::new(img).is_file() {
                format!(
                    "this is the old `<AppImage>/AppRun` bug — edit {} and change Exec to: Exec={img}",
                    desktop.display()
                )
            } else {
                format!("Exec points to {bin}, which doesn't exist — reinstall the app")
            }
        } else {
            format!("Exec points to {bin}, which doesn't exist — reinstall the app")
        };
        issues.push(HealthIssue {
            kind: HealthKind::BrokenExec,
            message: format!("'{}' menu entry points at a missing program: {bin}", inst.name),
            hint,
        });
    }
    issues
}

/// Ledger entries whose files no longer exist (and aren't external, where
/// dropin doesn't own the files).
fn check_orphaned(installs: &[Install]) -> Vec<HealthIssue> {
    installs
        .iter()
        .filter(|i| !i.external)
        .filter(|i| {
            // Orphaned when the install root is gone, or when every
            // tracked file is gone.
            let root_gone = i
                .install_root
                .as_ref()
                .map(|r| !Path::new(r).exists())
                .unwrap_or(false);
            root_gone || (!i.files.is_empty() && i.files.iter().all(|f| !Path::new(f).exists()))
        })
        .map(|i| HealthIssue {
            kind: HealthKind::Orphaned,
            message: format!(
                "'{}': its files are gone (deleted outside dropin?)",
                i.name
            ),
            hint: format!("run `dropin remove {}` to clear the ledger entry", i.name),
        })
        .collect()
}

/// Normalize an app name for comparison: lowercase, alphanumeric only.
fn norm(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// The display Name= from an install's managed .desktop file, if any.
fn desktop_name(dirs: &UserDirs, inst: &Install) -> Option<String> {
    inst.files
        .iter()
        .find(|f| {
            let p = Path::new(f);
            p.starts_with(&dirs.desktop_entries)
                && p.extension().map(|e| e == "desktop").unwrap_or(false)
        })
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| {
            t.lines().find_map(|l| {
                let l = l.trim();
                l.strip_prefix("Name=")
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
        })
}

/// Two ledger entries that look like the same app (same menu name or one
/// name containing the other).
fn check_duplicates(dirs: &UserDirs, installs: &[Install]) -> Vec<HealthIssue> {
    let mut by_name: HashMap<String, Vec<&Install>> = HashMap::new();
    for inst in installs {
        let key = desktop_name(dirs, inst)
            .map(|n| norm(&n))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| norm(&inst.name));
        by_name.entry(key).or_default().push(inst);
    }
    let mut issues = Vec::new();
    for group in by_name.values().filter(|g| g.len() > 1) {
        let names: Vec<_> = group.iter().map(|i| format!("'{}' ({})", i.name, i.format)).collect();
        issues.push(HealthIssue {
            kind: HealthKind::Duplicate,
            message: format!("possible duplicate install: {}", names.join(", ")),
            hint: "keep the one you use; `dropin remove <name>` the other".into(),
        });
    }
    issues
}

/// System .desktop directories whose entries a user-level file can shadow
/// (XDG: same basename in ~/.local/share/applications wins).
fn system_desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/var/lib/snapd/desktop/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ];
    let home = crate::home_dir();
    dirs.push(home.join(".local/share/flatpak/exports/share/applications"));
    dirs.into_iter().filter(|d| d.is_dir()).collect()
}

/// A dropin-managed menu entry that hides a system entry of the same name.
/// Dangerous because deleting the AppImage leaves a dead launcher hiding
/// the working system app.
fn check_shadowed(
    dirs: &UserDirs,
    sys: &[PathBuf],
    installs: &[Install],
) -> Vec<HealthIssue> {
    if sys.is_empty() {
        return Vec::new();
    }
    let mut issues = Vec::new();
    for inst in installs {
        for f in &inst.files {
            let p = Path::new(f);
            if !p.starts_with(&dirs.desktop_entries) {
                continue;
            }
            if p.extension().map(|e| e != "desktop").unwrap_or(true) {
                continue;
            }
            let base = match p.file_name().and_then(|n| n.to_str()) {
                Some(b) => b,
                None => continue,
            };
            if sys.iter().any(|d| d.join(base).exists()) {
                issues.push(HealthIssue {
                    kind: HealthKind::Shadowed,
                    message: format!(
                        "'{}': ~/.local/share/applications/{base} shadows a system menu entry",
                        inst.name
                    ),
                    hint: "if you delete the app's files by hand, the menu keeps a dead launcher hiding the system app — use `dropin remove` instead".into(),
                });
                break;
            }
        }
    }
    issues
}

/// Is `name` installed via a system package manager too?
fn installed_via_system(name: &str) -> Vec<&'static str> {
    let mut via = Vec::new();
    // dpkg (Debian/Ubuntu)
    if have("dpkg-query") {
        let out = std::process::Command::new("dpkg-query")
            .args(["-W", "-f=${db:Status-Status}", name])
            .output();
        if out.map(|o| String::from_utf8_lossy(&o.stdout).trim() == "installed").unwrap_or(false) {
            via.push("dpkg");
        }
    }
    // rpm (Fedora/openSUSE)
    if have("rpm") {
        let ok = std::process::Command::new("rpm")
            .args(["-q", name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            via.push("rpm");
        }
    }
    via
}

fn flatpak_apps() -> Vec<String> {
    if !have("flatpak") {
        return Vec::new();
    }
    std::process::Command::new("flatpak")
        .args(["list", "--app", "--columns=application"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_lowercase())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The same app installed both via dropin and via a system package manager
/// (apt/dnf/flatpak/snap) → duplicate menu entries, version confusion.
fn check_cross_source(dirs: &UserDirs, installs: &[Install]) -> Vec<HealthIssue> {
    let flatpaks = flatpak_apps();
    let mut issues = Vec::new();
    for inst in installs {
        let candidates = [
            inst.name.clone(),
            desktop_name(dirs, inst).unwrap_or_default(),
        ];
        let mut sources: Vec<String> = Vec::new();
        for cand in &candidates {
            if cand.is_empty() {
                continue;
            }
            for s in installed_via_system(cand) {
                let s = s.to_string();
                if !sources.contains(&s) {
                    sources.push(s);
                }
            }
            // flatpak ids are reverse-DNS; match on the last component.
            let n = norm(cand);
            if !sources.contains(&"flatpak".to_string())
                && flatpaks
                    .iter()
                    .any(|f| f.rsplit('.').next().map(norm) == Some(n.clone()))
            {
                sources.push("flatpak".to_string());
            }
        }
        if !sources.is_empty() {
            issues.push(HealthIssue {
                kind: HealthKind::CrossSource,
                message: format!(
                    "'{}' is also installed via {} — duplicate menu entries",
                    inst.name,
                    sources.join(", ")
                ),
                hint: "keep one; `dropin remove <name>` the dropin copy or uninstall the system package".into(),
            });
        }
    }
    issues
}


/// Conservative guard for `remove --purge`: true when the app name is still
/// provided by another ledger entry, a system package, or a system .desktop
/// entry — in which case shared config dirs must not be trashed.
pub(crate) fn name_still_provided(installs: &[Install], inst: &Install) -> bool {
    let n = norm(&inst.name);
    // Another dropin install with the same normalized name.
    if installs
        .iter()
        .any(|o| o.name != inst.name && norm(&o.name) == n)
    {
        return true;
    }
    // A system package manager still provides it.
    if !installed_via_system(&inst.name).is_empty() {
        return true;
    }
    // A system .desktop entry shares a basename with one of ours.
    let basenames: Vec<_> = inst
        .files
        .iter()
        .filter_map(|f| Path::new(f).file_name().map(|b| b.to_owned()))
        .collect();
    !basenames.is_empty()
        && system_desktop_dirs().iter().any(|d| {
            basenames.iter().any(|b| d.join(b).exists())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dropin-health-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn test_dirs(base: &Path) -> UserDirs {
        UserDirs {
            applications: base.join("Applications"),
            opt: base.join("opt"),
            bin: base.join("bin"),
            desktop_entries: base.join("applications"),
            icons: base.join("icons"),
            data: base.join("data"),
        }
    }

    fn install(name: &str, files: Vec<String>) -> Install {
        Install {
            id: 1,
            name: name.into(),
            format: "AppImage".into(),
            version: None,
            source_path: "/tmp/x".into(),
            source_sha256: None,
            install_root: None,
            external: false,
            update_info: None,
            verified: false,
            github_repo: None,
            signing_key: None,
            created_at: String::new(),
            files,
        }
    }

    #[test]
    fn norm_strips_noise() {
        assert_eq!(norm("Foo Bar 2.0!"), "foobar20");
        assert_eq!(norm("  VLC  "), "vlc");
    }

    #[test]
    fn flags_orphaned_entries() {
        let d = tmpdir("orphan");
        let gone = install("gone", vec!["/nonexistent/dropin-health-file".into()]);
        let issues = check_orphaned(&[gone]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, HealthKind::Orphaned);

        // An install with a live file is fine.
        let live_file = d.join("app.AppImage");
        std::fs::write(&live_file, b"x").unwrap();
        let live = install(
            "live",
            vec![live_file.display().to_string()],
        );
        assert!(check_orphaned(&[live]).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn flags_duplicate_menu_names() {
        let d = tmpdir("dup");
        let dirs = test_dirs(&d);
        std::fs::create_dir_all(&dirs.desktop_entries).unwrap();
        for (slug, name) in [("foo-ai", "Foo App"), ("foo-tb", "foo app")] {
            let p = dirs.desktop_entries.join(format!("{slug}.desktop"));
            std::fs::write(&p, format!("[Desktop Entry]\nName={name}\n")).unwrap();
        }
        let a = install(
            "foo-ai",
            vec![dirs.desktop_entries.join("foo-ai.desktop").display().to_string()],
        );
        let b = install(
            "foo-tb",
            vec![dirs.desktop_entries.join("foo-tb.desktop").display().to_string()],
        );
        let issues = check_duplicates(&dirs, &[a, b]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, HealthKind::Duplicate);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn flags_shadowed_system_entry() {
        let d = tmpdir("shadow");
        let dirs = test_dirs(&d);
        std::fs::create_dir_all(&dirs.desktop_entries).unwrap();
        let sys = d.join("sysapps");
        std::fs::create_dir_all(&sys).unwrap();
        // Same basename in both places: user entry shadows the system one.
        std::fs::write(dirs.desktop_entries.join("thing.desktop"), b"[Desktop Entry]\n").unwrap();
        std::fs::write(sys.join("thing.desktop"), b"[Desktop Entry]\n").unwrap();
        let inst = install(
            "thing",
            vec![dirs.desktop_entries.join("thing.desktop").display().to_string()],
        );
        let issues = check_shadowed(&dirs, &[sys], &[inst]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, HealthKind::Shadowed);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_shadow_without_system_match() {
        let d = tmpdir("noshadow");
        let dirs = test_dirs(&d);
        std::fs::create_dir_all(&dirs.desktop_entries).unwrap();
        let sys = d.join("sysapps");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(dirs.desktop_entries.join("mine.desktop"), b"[Desktop Entry]\n").unwrap();
        let inst = install(
            "mine",
            vec![dirs.desktop_entries.join("mine.desktop").display().to_string()],
        );
        assert!(check_shadowed(&dirs, &[sys], &[inst]).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detects_apprun_exec_bug() {
        let d = tmpdir("brokenexec");
        let dirs = test_dirs(&d);
        std::fs::create_dir_all(&dirs.desktop_entries).unwrap();
        std::fs::create_dir_all(&dirs.applications).unwrap();
        // The AppImage itself exists; the old bug wrote Exec=<image>/AppRun.
        let img = dirs.applications.join("app.AppImage");
        std::fs::write(&img, b"fake").unwrap();
        std::fs::write(
            dirs.desktop_entries.join("dropin-myapp.desktop"),
            format!("[Desktop Entry]\nName=MyApp\nExec={}/AppRun\n", img.display()),
        )
        .unwrap();
        let inst = install("myapp", vec![img.display().to_string()]);
        let issues = check_broken_exec(&dirs, &[inst]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, HealthKind::BrokenExec);
        assert!(issues[0].hint.contains(&img.display().to_string()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn healthy_exec_is_quiet() {
        let d = tmpdir("healthyexec");
        let dirs = test_dirs(&d);
        std::fs::create_dir_all(&dirs.desktop_entries).unwrap();
        std::fs::create_dir_all(&dirs.applications).unwrap();
        let img = dirs.applications.join("app.AppImage");
        std::fs::write(&img, b"fake").unwrap();
        std::fs::write(
            dirs.desktop_entries.join("dropin-myapp.desktop"),
            format!("[Desktop Entry]\nName=MyApp\nExec={}\n", img.display()),
        )
        .unwrap();
        let inst = install("myapp", vec![img.display().to_string()]);
        assert!(check_broken_exec(&dirs, &[inst]).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
