//! .deb handler. Default: rootless extraction — unpack `data.tar.*` into
//! ~/.local/opt/<pkg>, adopt the bundled .desktop files, symlink usr/bin/*.
//! With `--system`, delegate to apt via polkit (tracked as external).
//!
//! Pure Rust: parses the `ar` container format natively, no external tools.

use super::{assert_safe_member, sha256_of, InstallOptions, InstallOutcome, InstallPlan};
use crate::desktop::{adopt_desktop_file, parse_desktop_file};
use crate::{detect, slugify, Error, Result, UserDirs};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A member of an `ar` archive (the .deb container format).
struct ArMember {
    name: String,
    offset: u64,
    size: u64,
}

/// Parse the `ar` container format in pure Rust.
/// Format: "!<arch>\n" magic, then entries with 60-byte headers.
fn ar_members(src: &Path) -> Result<Vec<ArMember>> {
    let mut f = std::fs::File::open(src)
        .map_err(|e| Error::Command(format!("Couldn't read the .deb file.\n\nDetails: {e}")))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)
        .map_err(|e| Error::Command(format!("Couldn't read the .deb file.\n\nDetails: {e}")))?;
    if &magic != b"!<arch>\n" {
        return Err(Error::Command(
            "This doesn't look like a .deb file (bad ar magic).".into(),
        ));
    }
    let mut members = Vec::new();
    loop {
        let mut header = [0u8; 60];
        match f.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                return Err(Error::Command(format!(
                    "Couldn't read the .deb container.\n\nDetails: {e}"
                )))
            }
        }
        // Header layout: name(16) timestamp(12) uid(6) gid(6) mode(8) size(10) end(2)
        let name = String::from_utf8_lossy(&header[0..16])
            .trim()
            .trim_end_matches('/')
            .to_string();
        let size: u64 = String::from_utf8_lossy(&header[48..58])
            .trim()
            .parse()
            .map_err(|_| Error::Command("Couldn't parse .deb member size.".into()))?;
        // Verify the header terminator.
        if &header[58..60] != b"\x60\x0a" {
            return Err(Error::Command(
                "Couldn't parse the .deb container (bad member header).".into(),
            ));
        }
        let offset = f
            .stream_position()
            .map_err(|e| Error::Command(format!("Couldn't read the .deb file.\n\nDetails: {e}")))?;
        members.push(ArMember { name, offset, size });
        // Skip data + padding to even boundary.
        let skip = size + (size % 2);
        f.seek(SeekFrom::Current(skip as i64))
            .map_err(|e| Error::Command(format!("Couldn't read the .deb file.\n\nDetails: {e}")))?;
    }
    Ok(members)
}

/// Extract one `ar` member to a temp file, return its path.
fn ar_extract_member(src: &Path, member: &ArMember) -> Result<PathBuf> {
    let mut f = std::fs::File::open(src)
        .map_err(|e| Error::Command(format!("Couldn't read the .deb file.\n\nDetails: {e}")))?;
    f.seek(SeekFrom::Start(member.offset))
        .map_err(|e| Error::Command(format!("Couldn't read the .deb file.\n\nDetails: {e}")))?;
    let tmp = crate::unique_temp_dir("dropin-deb-member");
    let mut out = std::fs::File::create(&tmp)
        .map_err(|e| Error::Command(format!("Couldn't write temp file.\n\nDetails: {e}")))?;
    std::io::copy(&mut f.by_ref().take(member.size), &mut out)
        .map_err(|e| Error::Command(format!("Couldn't extract .deb member.\n\nDetails: {e}")))?;
    Ok(tmp)
}

/// Read a single file from a tar archive, return its text content.
fn read_tar_member<R: std::io::Read>(reader: R, want: &str) -> Option<String> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        let path = entry.path().ok()?;
        let name = path.to_string_lossy();
        // Match "control" or "./control".
        if name == want || name == format!("./{want}") {
            let mut content = String::new();
            use std::io::Read;
            entry.read_to_string(&mut content).ok()?;
            return Some(content);
        }
    }
    None
}

fn deb_control_field(src: &Path, field: &str) -> Option<String> {
    let members = ar_members(src).ok()?;
    let control = members.iter().find(|m| m.name.starts_with("control.tar"))?;
    let tmp = ar_extract_member(src, control).ok()?;
    // Extract the control file from the tar using pure-Rust.
    let result = (|| -> Option<String> {
        let comp = super::tarball::detect_tar_compression(&tmp).ok()?;
        // For control.tar we just need the raw bytes; use list + extract one file.
        // Simpler: read the tar and find the control file manually.
        let file = std::fs::File::open(&tmp).ok()?;
        let text = match comp {
            super::tarball::TarCompression::Gzip => {
                let gz = flate2::read::GzDecoder::new(file);
                read_tar_member(gz, "control")?
            }
            super::tarball::TarCompression::Xz => {
                let xz = liblzma::read::XzDecoder::new(file);
                read_tar_member(xz, "control")?
            }
            super::tarball::TarCompression::Zstd => {
                let zst = zstd::Decoder::new(file).ok()?;
                read_tar_member(zst, "control")?
            }
            _ => read_tar_member(file, "control")?,
        };
        Some(text)
    })();
    let _ = std::fs::remove_file(&tmp);
    let text = result?;
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
}

pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    if opts.system {
        return install_system(src);
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
    // Pure Rust: parse ar container, extract member, unpack tar natively.
    let members = ar_members(src)?;
    let data_member = members
        .iter()
        .find(|m| m.name.starts_with("data.tar"))
        .ok_or_else(|| Error::Unsupported("no data.tar.* in deb".into()))?;
    let tmp = ar_extract_member(src, data_member)?;
    // Safety: list first, reject escapes.
    let listing = super::tarball::list_members_tar(&tmp)?;
    for line in &listing {
        assert_safe_member(line.trim_start_matches("./"))?;
    }
    std::fs::create_dir_all(&root)?;
    let extract_result = super::tarball::extract_tar(&tmp, &root, 0);
    let _ = std::fs::remove_file(&tmp);
    if let Err(e) = extract_result {
        let _ = std::fs::remove_dir_all(&root);
        return Err(e);
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
    let s = src
        .to_str()
        .ok_or_else(|| Error::Unsupported("bad path".into()))?;
    // Detect distro family from /etc/os-release ID_LIKE / ID.
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let (cmd, args): (&str, Vec<&str>) = if os_release.contains("ID_LIKE=debian")
        || os_release.contains("\nID=debian")
        || os_release.contains("\nID=ubuntu")
    {
        ("pkexec", vec!["apt", "install", "-y", s])
    } else if os_release.contains("fedora")
        || os_release.contains("rhel")
        || os_release.contains("ID_LIKE=\"rhel")
    {
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
