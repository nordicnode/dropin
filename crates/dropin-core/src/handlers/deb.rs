//! .deb handler. Default: rootless extraction — unpack `data.tar.*` into
//! ~/.local/opt/<pkg>, adopt the bundled .desktop files, symlink usr/bin/*.
//! With `--system`, delegate to apt via polkit (tracked as external).

use super::{assert_safe_member, sha256_of, InstallOptions, InstallOutcome, InstallPlan};
use crate::desktop::{adopt_desktop_file, parse_desktop_file};
use crate::{detect, have, slugify, Error, Result, UserDirs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn deb_control_field(src: &Path, field: &str) -> Option<String> {
    let s = src.to_str()?;
    let members = crate::run("ar", &["t", s]).ok()?;
    let control_member = members
        .lines()
        .map(str::trim)
        .find(|m| m.starts_with("control.tar"))?;
    // Dump to a temp file first: tar sniffs compression from files, not pipes.
    let tmp = std::env::temp_dir().join(format!("dropin-ctrl-{}", std::process::id()));
    std::process::Command::new("sh")
        .args(["-c", &format!("ar p {s} {control_member} > {}", tmp.display())])
        .status()
        .ok()?;
    // Member may be stored as "./control" or "control".
    let mut text = None;
    for member in ["./control", "control"] {
        let out = std::process::Command::new("tar")
            .args(["-xO", "-f", tmp.to_str()?, member])
            .output()
            .ok()?;
        if out.status.success() {
            text = Some(String::from_utf8_lossy(&out.stdout).to_string());
            break;
        }
    }
    let _ = std::fs::remove_file(&tmp);
    let text = text?;
    text.lines().find_map(|l| {
        l.strip_prefix(&format!("{field}:")) // "Package: foo"
            .or_else(|| l.strip_prefix(&format!("{field} :")))
            .map(|v| v.trim().to_string())
    })
}

pub fn package_name(src: &Path) -> String {
    deb_control_field(src, "Package")
        .map(|p| slugify(&p))
        .unwrap_or_else(|| slugify(&detect::detect(src).map(|d| d.name_hint).unwrap_or_default()))
}

pub fn plan(src: &Path) -> Result<InstallPlan> {
    let pkg = package_name(src);
    if have("ar") {
        Ok(InstallPlan {
            app_name: pkg.clone(),
            version: deb_control_field(src, "Version"),
            steps: vec![
                format!("Extract package payload to ~/.local/opt/{pkg} (no root needed)"),
                "Adopt bundled application menu entries (paths rewritten)".into(),
                "Symlink /usr/bin programs into ~/.local/bin".into(),
            ],
            needs_root: false,
            needs_confirmation: false,
        })
    } else {
        Err(Error::Command(
            "the `ar` tool is required to handle .deb files".into(),
        ))
    }
}

pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    if opts.system {
        return install_system(src);
    }
    if !have("ar") {
        return Err(Error::Command("the `ar` tool is required".into()));
    }
    let dirs = UserDirs::new();
    dirs.ensure()?;
    let pkg = package_name(src);
    let root = dirs.opt.join(&pkg);
    if root.exists() {
        return Err(Error::Refused(format!(
            "~/.local/opt/{pkg} already exists — remove it first with `dropin remove {pkg}`"
        )));
    }

    // Extract data.tar.* -> root, preserving the usr/ tree inside root.
    let s = src.to_str().ok_or_else(|| Error::Unsupported("bad path".into()))?;
    let members = crate::run("ar", &["t", s])?;
    let data_member = members
        .lines()
        .map(str::trim)
        .find(|m| m.starts_with("data.tar"))
        .ok_or_else(|| Error::Unsupported("no data.tar.* in deb".into()))?;
    // Dump the member to a temp file: tar sniffs compression reliably from a
    // file, but not always from a pipe.
    let tmp = std::env::temp_dir().join(format!("dropin-data-{}", std::process::id()));
    let dump = std::process::Command::new("sh")
        .args([
            "-c",
            &format!("ar p {s} {data_member} > {}", tmp.display()),
        ])
        .status()
        .map_err(|e| Error::Command(format!("reading deb payload: {e}")))?;
    if !dump.success() {
        return Err(Error::Command("reading deb payload failed".into()));
    }
    // Safety: list first, reject escapes.
    let listing = crate::run("tar", &["-tf", tmp.to_str().unwrap()])?;
    for line in listing.lines() {
        assert_safe_member(line.trim_start_matches("./"))?;
    }
    std::fs::create_dir_all(&root)?;
    let status = std::process::Command::new("tar")
        .args(["-xf", tmp.to_str().unwrap(), "-C", root.to_str().unwrap()])
        .status()
        .map_err(|e| Error::Command(format!("extracting deb: {e}")))?;
    let _ = std::fs::remove_file(&tmp);
    if !status.success() {
        let _ = std::fs::remove_dir_all(&root);
        return Err(Error::Command("extracting deb payload failed".into()));
    }

    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&root)
        .into_iter()
        .flatten()
        .map(|e| e.path().to_path_buf())
        .collect();

    // Adopt bundled .desktop files (usr/share/applications/*.desktop).
    let apps_dir = root.join("usr/share/applications");
    if apps_dir.is_dir() {
        let entries: Vec<PathBuf> = std::fs::read_dir(&apps_dir)
            .map(|r| {
                r.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|e| e == "desktop").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        for df in entries {
            if parse_desktop_file(&df).is_ok() {
                let stem = df
                    .file_stem()
                    .map(|s| slugify(&s.to_string_lossy()))
                    .unwrap_or_else(|| pkg.clone());
                match adopt_desktop_file(&df, &stem, &pkg, &root) {
                    Ok((_, mut extra)) => paths.append(&mut extra),
                    Err(e) => eprintln!("warning: skipping menu entry {}: {e}", df.display()),
                }
            }
        }
    }

    // Symlink usr/bin/* and usr/local/bin/* into ~/.local/bin.
    for bindir in ["usr/bin", "usr/local/bin", "bin", "opt"] {
        let dir = root.join(bindir);
        if !dir.is_dir() {
            continue;
        }
        let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|r| r.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        for bin in entries {
            if bin.is_file() {
                // Ensure executables stay executable after ar/tar round-trip.
                if bindir != "opt" {
                    let mut perms = std::fs::metadata(&bin)?.permissions();
                    perms.set_mode(perms.mode() | 0o111);
                    let _ = std::fs::set_permissions(&bin, perms);
                }
                let link = dirs.bin.join(bin.file_name().unwrap());
                if link.exists() || link.is_symlink() {
                    eprintln!(
                        "warning: ~/.local/bin/{} already exists, skipping",
                        link.file_name().unwrap().to_string_lossy()
                    );
                    continue;
                }
                std::os::unix::fs::symlink(&bin, &link)?;
                paths.push(link);
            }
        }
    }

    let _ = sha256_of(src).ok();
    Ok(InstallOutcome {
        name: pkg,
        version: deb_control_field(src, "Version"),
        install_root: Some(root),
        paths,
        external: false,
        update_info: None,
    })
}

/// System install via the distro package manager with a polkit prompt.
fn install_system(src: &Path) -> Result<InstallOutcome> {
    let s = src.to_str().ok_or_else(|| Error::Unsupported("bad path".into()))?;
    // Detect distro family from /etc/os-release ID_LIKE / ID.
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let (cmd, args): (&str, Vec<&str>) = if os_release.contains("ID_LIKE=debian") || os_release.contains("\nID=debian") || os_release.contains("\nID=ubuntu") {
        ("pkexec", vec!["apt", "install", "-y", s])
    } else if os_release.contains("fedora") || os_release.contains("rhel") || os_release.contains("ID_LIKE=\"rhel") {
        ("pkexec", vec!["dnf", "install", "-y", s])
    } else if os_release.contains("arch") {
        ("pkexec", vec!["pacman", "-U", "--noconfirm", s])
    } else {
        return Err(Error::Refused(
            "no supported system package manager detected for --system install".into(),
        ));
    };
    let status = std::process::Command::new(cmd)
        .args(&args)
        .status()
        .map_err(|e| Error::Command(format!("system install failed: {e}")))?;
    if !status.success() {
        return Err(Error::Command("system package install failed".into()));
    }
    let pkg = package_name(src);
    Ok(InstallOutcome {
        name: pkg,
        version: None,
        install_root: None,
        paths: vec![],
        external: true, // the system package manager owns these files now
        update_info: None,
    })
}
