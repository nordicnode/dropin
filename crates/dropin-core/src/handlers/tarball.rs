//! Tarball/zip handler: sniff whether an archive *is an app*, and if so
//! install it rootless into ~/.local/opt/<app>.

use super::{assert_safe_member, sha256_of, InstallOptions, InstallOutcome, InstallPlan};
use crate::desktop::{adopt_desktop_file, generate_entry, parse_desktop_file};
use crate::{detect, have, slugify, Error, Result, UserDirs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn is_zip(src: &Path) -> bool {
    detect::detect(src)
        .map(|d| d.format == detect::Format::Zip)
        .unwrap_or(false)
}

/// List archive members (one path per line, no leading ./).
fn list_members(src: &Path) -> Result<Vec<String>> {
    let s = src
        .to_str()
        .ok_or_else(|| Error::Unsupported("bad path".into()))?;
    let out = if is_zip(src) {
        if !have("unzip") {
            return Err(Error::Command("The `unzip` tool isn't installed, so dropin can't read zip files. Install it with your package manager (e.g. `sudo apt install unzip`).".into()));
        }
        match crate::run("unzip", &["-Z", "-1", s]) {
            Ok(o) => o,
            Err(e) => {
                let detail = e.to_string();
                if detail.contains("cannot find") || detail.contains("No such file") {
                    return Err(Error::Command(
                        "Couldn't read the zip file. It may have been moved or deleted.".into(),
                    ));
                }
                return Err(Error::Command(format!(
                    "Couldn't read the zip file. It may be corrupted or not actually a zip archive.\n\nDetails: {detail}"
                )));
            }
        }
    } else {
        crate::run("tar", &["-tf", s])?
    };
    let mut members: Vec<String> = out
        .lines()
        .map(|l| l.trim_start_matches("./").to_string())
        .filter(|l| !l.is_empty())
        .collect();
    for m in &members {
        assert_safe_member(m)?;
    }
    members.sort();
    members.dedup();
    Ok(members)
}

fn extract(src: &Path, dest: &Path, strip: usize) -> Result<()> {
    let s = src
        .to_str()
        .ok_or_else(|| Error::Unsupported("bad path".into()))?;
    let d = dest
        .to_str()
        .ok_or_else(|| Error::Unsupported("bad path".into()))?;
    std::fs::create_dir_all(dest)?;
    if is_zip(src) {
        // unzip has no strip-components; emulate by extracting then moving.
        // Note: no -q flag, so unzip's error messages reach stderr if it fails.
        let tmp = dest.join(".dropin-unzip-tmp");
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)?;
        }
        std::fs::create_dir_all(&tmp)?;
        if let Err(e) = crate::run("unzip", &["-o", s, "-d", tmp.to_str().unwrap()]) {
            // Translate the technical failure into something actionable.
            // Common causes: corrupted/incomplete download, disk full, or
            // an archive format unzip can't handle.
            let detail = e.to_string();
            // Clean up the temp dir so a retry starts fresh.
            let _ = std::fs::remove_dir_all(&tmp);
            if detail.contains("cannot find") || detail.contains("No such file") {
                return Err(Error::Command(
                    "Couldn't read the zip file. It may have been moved or deleted.".into(),
                ));
            }
            if detail.contains("disk") || detail.contains("space") || detail.contains("No space") {
                return Err(Error::Command(
                    "Not enough disk space to extract the zip file.".into(),
                ));
            }
            // Generic: likely a corrupted or unsupported archive.
            return Err(Error::Command(format!(
                "Couldn't extract the zip file. It may be corrupted or incomplete — try downloading it again.\n\nDetails: {detail}"
            )));
        }
        move_stripped(&tmp, dest, strip)?;
        std::fs::remove_dir_all(&tmp)?;
    } else {
        let strip_arg = format!("--strip-components={strip}");
        crate::run("tar", &["-xf", s, "-C", d, &strip_arg])?;
    }
    Ok(())
}

fn move_stripped(tmp: &Path, dest: &Path, strip: usize) -> Result<()> {
    let mut cur = tmp.to_path_buf();
    for _ in 0..strip {
        let entries: Vec<_> = std::fs::read_dir(&cur)?.flatten().collect();
        if entries.len() == 1 && entries[0].path().is_dir() {
            cur = entries[0].path();
        } else {
            break;
        }
    }
    for e in std::fs::read_dir(&cur)?.flatten() {
        let target = dest.join(e.file_name());
        if target.exists() {
            std::fs::remove_dir_all(&target).or_else(|_| std::fs::remove_file(&target))?;
        }
        std::fs::rename(e.path(), &target)?;
    }
    Ok(())
}

/// If every member sits under one top-level dir, return it (for stripping).
fn single_top_dir(members: &[String]) -> Option<String> {
    let tops: Vec<&str> = members
        .iter()
        .map(|m| m.split('/').next().unwrap_or(""))
        .filter(|t| !t.is_empty())
        .collect();
    let first = *tops.first()?;
    if tops.iter().all(|t| *t == first) && members.iter().any(|m| m.contains('/')) {
        Some(first.to_string())
    } else {
        None
    }
}

#[derive(Debug)]
struct Sniff {
    desktop_files: Vec<String>,
    /// member path -> is it an ELF executable?
    executables: Vec<String>,
    has_app_asar: bool,
}

fn sniff(_members: &[String], src: &Path, strip: usize) -> Result<Sniff> {
    // To check ELF magic without extracting everything, extract to a scratch
    // dir (cheap relative to the install itself) and inspect.
    let scratch = std::env::temp_dir().join(format!("dropin-sniff-{}", std::process::id()));
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)?;
    }
    extract(src, &scratch, strip)?;

    let mut desktop_files = Vec::new();
    let mut executables = Vec::new();
    let mut has_app_asar = false;
    for entry in walkdir::WalkDir::new(&scratch).into_iter().flatten() {
        let rel = entry
            .path()
            .strip_prefix(&scratch)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();
        if rel.ends_with("app.asar") {
            has_app_asar = true;
        }
        if entry
            .path()
            .extension()
            .map(|e| e == "desktop")
            .unwrap_or(false)
        {
            desktop_files.push(rel.clone());
        }
        if entry.file_type().is_file() && is_elf_executable(entry.path()) {
            executables.push(rel);
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(Sniff {
        desktop_files,
        executables,
        has_app_asar,
    })
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
    // ELF magic alone is enough (permission bits can be lost in zips).
    if buf == [0x7f, b'E', b'L', b'F'] {
        return true;
    }
    // Otherwise: a script counts only with a shebang AND an exec bit,
    // so we don't mistake documentation for launchers.
    if !(buf[0] == b'#' && buf[1] == b'!') {
        return false;
    }
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn looks_like_app(sniff: &Sniff) -> bool {
    !sniff.desktop_files.is_empty() || !sniff.executables.is_empty() || sniff.has_app_asar
}

pub fn plan(src: &Path) -> Result<InstallPlan> {
    let d = detect::detect(src)?;
    let members = list_members(src)?;
    let strip = single_top_dir(&members).map(|_| 1).unwrap_or(0);
    let sniff = sniff(&members, src, strip)?;
    if !looks_like_app(&sniff) {
        return Err(Error::Refused(format!(
            "{} looks like data, not an application (no executables or .desktop files found); opening with your archive manager instead",
            src.display()
        )));
    }
    let slug = slugify(&d.name_hint);
    Ok(InstallPlan {
        app_name: d.name_hint.clone(),
        version: None,
        steps: vec![
            format!("Extract to ~/.local/opt/{slug}"),
            format!(
                "Link executables into ~/.local/bin ({})",
                sniff.executables.len()
            ),
            "Adopt or generate an application menu entry".into(),
        ],
        needs_root: false,
        needs_confirmation: false,
    })
}

pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    let d = detect::detect(src)?;
    let members = list_members(src)?;
    let strip = single_top_dir(&members).map(|_| 1).unwrap_or(0);
    let sniff = sniff(&members, src, strip)?;
    if !looks_like_app(&sniff) {
        return Err(Error::Refused(format!(
            "{} looks like data, not an application",
            src.display()
        )));
    }

    let dirs = UserDirs::new();
    dirs.ensure()?;
    let slug = slugify(&d.name_hint);
    let root = dirs.opt.join(&slug);
    if root.exists() {
        return Err(Error::Refused(format!(
            "~/.local/opt/{slug} already exists — remove it first with `dropin remove {slug}`"
        )));
    }
    let _ = opts; // (keep_source is meaningless for archives; the file stays)

    extract(src, &root, strip)?;
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&root)
        .into_iter()
        .flatten()
        .map(|e| e.path().to_path_buf())
        .collect();
    // WalkDir yields the root itself first; keep it (removal deletes it whole).

    // Choose the main binary: name match first, then shallowest executable.
    let main_bin = choose_binary(&sniff, &slug, &d.name_hint).map(|rel| root.join(&rel));

    // Symlink executables into ~/.local/bin (just the main one + any in bin/).
    let mut linked = 0;
    if let Some(bin) = &main_bin {
        link_bin(bin, &dirs, &mut paths)?;
        linked += 1;
    }

    // Menu entry: adopt the best bundled .desktop, else generate one.
    if let Some(df_rel) = choose_desktop(&sniff, &slug) {
        let df_path = root.join(&df_rel);
        if parse_desktop_file(&df_path).is_ok() {
            let (_, mut extra) = adopt_desktop_file(&df_path, &slug, &d.name_hint, &root)?;
            paths.append(&mut extra);
        } else if let Some(bin) = &main_bin {
            let (_, mut extra) = generate_entry(&d.name_hint, &slug, bin)?;
            paths.append(&mut extra);
        }
    } else if let Some(bin) = &main_bin {
        let (_, mut extra) = generate_entry(&d.name_hint, &slug, bin)?;
        paths.append(&mut extra);
    }
    let _ = linked;

    let sha = sha256_of(src).ok();
    let _ = sha;
    Ok(InstallOutcome {
        name: slug,
        version: None,
        install_root: Some(root),
        paths,
        external: false,
        update_info: None,
    })
}

/// Prefer a .desktop whose filename matches the slug, else the shallowest.
fn choose_desktop(sniff: &Sniff, slug: &str) -> Option<String> {
    if sniff.desktop_files.is_empty() {
        return None;
    }
    let mut cands = sniff.desktop_files.clone();
    cands.sort_by_key(|p| {
        let name_match = Path::new(p)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase().contains(slug))
            .unwrap_or(false);
        (!name_match, p.matches('/').count(), p.len())
    });
    cands.into_iter().next()
}

/// Prefer an executable whose name matches the app, else the shallowest one
/// (excluding anything under lib/ or resources/).
fn choose_binary(sniff: &Sniff, slug: &str, name_hint: &str) -> Option<String> {
    if sniff.executables.is_empty() {
        return None;
    }
    let mut cands = sniff.executables.clone();
    cands.sort_by_key(|p| {
        let stem = Path::new(p)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let name_match = stem == slug || stem == name_hint.to_lowercase();
        let in_lib = p.contains("/lib/") || p.contains("/resources/");
        (!name_match, in_lib, p.matches('/').count(), p.len())
    });
    cands.into_iter().next()
}

fn link_bin(bin: &Path, dirs: &UserDirs, paths: &mut Vec<PathBuf>) -> Result<()> {
    let name = bin
        .file_name()
        .ok_or_else(|| Error::Unsupported("bad binary name".into()))?;
    let link = dirs.bin.join(name);
    if link.exists() || link.is_symlink() {
        std::fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(bin, &link)?;
    paths.push(link);
    Ok(())
}
