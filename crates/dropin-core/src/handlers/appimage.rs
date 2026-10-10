//! AppImage handler: move to ~/Applications, integrate into the menu.

use super::{InstallOptions, InstallOutcome, InstallPlan};
use crate::desktop::{adopt_desktop_file_appimage, generate_entry, parse_desktop_file};
use crate::{detect, slugify, Error, Result, UserDirs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn plan(src: &Path) -> Result<InstallPlan> {
    let d = detect::detect(src)?;
    Ok(InstallPlan {
        app_name: d.name_hint.clone(),
        version: None,
        steps: vec![
            format!("Move to ~/Applications/{}", src.file_name().unwrap().to_string_lossy()),
            "Make executable".into(),
            "Extract embedded icon and menu entry".into(),
            "Add to your application menu".into(),
        ],
        needs_root: false,
        needs_confirmation: false,
    })
}

/// Generation-based install: `~/Applications/<slug>/<version>/<file>`,
/// with an `active` symlink flipped atomically. Rollback is just repointing
/// the symlink; old generations are pruned to the last two.
pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    let d = detect::detect(src)?;
    if d.format != detect::Format::AppImage {
        return Err(Error::Unsupported(format!(
            "not an AppImage: {}",
            src.display()
        )));
    }
    let dirs = UserDirs::new();
    dirs.ensure()?;

    let file_name = src
        .file_name()
        .ok_or_else(|| Error::Unsupported("bad file name".into()))?;
    let file_name_str = file_name.to_string_lossy();

    // Pre-flight: architecture match. Warn if the filename names a
    // different arch (e.g. aarch64 AppImage on x86_64).
    preflight_arch(&file_name_str)?;

    // Pre-flight: disk space (need ~2x file size for copy + work).
    let src_size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    preflight_disk(&dirs.applications, src_size)?;

    let slug = slugify(&d.name_hint);
    let app_dir = dirs.applications.join(&slug);

    // Detect version early: X-AppImage-Version, else filename, else "unknown".
    // We need it before placing the file.
    let mut version: Option<String> = None;
    // Quick metadata peek for the version (full extraction happens after).
    // For now, try the filename; the embedded version refines it below.
    version = version_from_filename(&file_name.to_string_lossy())
        .or(version);
    let ver = version.clone().unwrap_or_else(|| "unknown".to_string());
    let ver_dir = app_dir.join(&ver);
    let dest = ver_dir.join(file_name);

    if dest.exists() || ver_dir.exists() {
        let hint = match crate::ledger::Ledger::open().and_then(|l| l.get(&slug)) {
            Ok(inst) => format!(
                "'{}' version {} is already installed. To replace it, remove it first or check for updates.",
                inst.name, ver
            ),
            Err(_) => format!(
                "{} already exists in ~/Applications but isn't tracked by dropin.",
                file_name.to_string_lossy()
            ),
        };
        return Err(Error::Refused(hint));
    }
    std::fs::create_dir_all(&ver_dir)?;

    if opts.keep_source {
        std::fs::copy(src, &dest)?;
    } else {
        std::fs::rename(src, &dest)?;
    }
    let mut perms = std::fs::metadata(&dest)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&dest, perms)?;

    // AppImages need FUSE to run. Warn now rather than leaving the user
    // with a menu entry that silently fails to launch.
    if !crate::health::have_fuse() {
        eprintln!(
            "warning: FUSE doesn't appear to be available — this AppImage may not launch.\n\
             Install FUSE for your distro (e.g. `apt install fuse libfuse2`) or run with --appimage-extract."
        );
    }

    // Flip `active` to the new generation atomically (symlink swap).
    let active = app_dir.join("active");
    flip_symlink(&active, &dest)?;

    let mut update_info: Option<String> = None;
    let mut extra_paths: Vec<PathBuf> = Vec::new();

    // Extract the embedded .desktop file + icon for menu integration.
    // The menu entry launches via the `active` symlink, so updates and
    // rollbacks never need to rewrite it.
    match extract_metadata(&dest) {
        Ok(meta) => {
            if let Some(df) = &meta.desktop_file {
                if let Ok(map) = parse_desktop_file(df) {
                    update_info = map.get("X-AppImage-UpdateInformation").cloned();
                    // Prefer the embedded version over the filename guess.
                    if let Some(v) = map.get("X-AppImage-Version") {
                        version = Some(v.clone());
                    }
                }
            }
            let (entry_path, mut extra) = match meta.desktop_file {
                Some(df) => adopt_desktop_file_appimage(&df, &slug, &d.name_hint, &active)?,
                None => generate_entry(&d.name_hint, &slug, &active)?,
            };
            let _ = entry_path;
            extra_paths.append(&mut extra);
            let _ = std::fs::remove_dir_all(meta.scratch);
        }
        Err(e) => {
            eprintln!("warning: could not extract AppImage metadata ({e}); using a generic menu entry");
            let (_, mut extra) = generate_entry(&d.name_hint, &slug, &active)?;
            extra_paths.append(&mut extra);
        }
    }

    // If the embedded version differs from the filename guess, rename the
    // version dir. (Rare; keeps the layout truthful.)
    let final_ver = version.clone().unwrap_or_else(|| "unknown".to_string());
    if final_ver != ver {
        let new_ver_dir = app_dir.join(&final_ver);
        if !new_ver_dir.exists() {
            std::fs::rename(&ver_dir, &new_ver_dir).ok();
        }
        let new_dest = new_ver_dir.join(file_name);
        flip_symlink(&active, &new_dest).ok();
    }

    // Ledger tracks the app dir (all generations + the active symlink live
    // inside it), plus the desktop entry and icon outside it.
    let mut paths = vec![app_dir];
    paths.extend(extra_paths);

    Ok(InstallOutcome {
        name: slug,
        version,
        install_root: None,
        paths,
        external: false,
        update_info,
    })
}

/// Pre-flight: refuse if the filename clearly targets a different CPU
/// architecture than this machine.
fn preflight_arch(file_name: &str) -> Result<()> {
    let lower = file_name.to_lowercase();
    let arch = std::env::consts::ARCH;

    // What arch does the filename claim? Order matters: check x86_64
    // before the bare "x86" so the substring doesn't false-positive.
    let claims = if lower.contains("x86_64") || lower.contains("amd64") || word_contains(&lower, "x64") {
        Some("x86_64")
    } else if lower.contains("aarch64") || lower.contains("arm64") {
        Some("aarch64")
    } else if lower.contains("i686") || lower.contains("i386") || lower.contains("x86") {
        // Reached only when "x86_64" wasn't found, so bare "x86" means 32-bit.
        Some("i686")
    } else {
        None
    };

    match claims {
        Some(claimed) if claimed != arch => Err(Error::Refused(format!(
            "this AppImage looks built for {claimed}, but this machine is {arch}"
        ))),
        _ => Ok(()),
    }
}

/// True when `word` appears as a standalone token (split on non-alphanumerics).
fn word_contains(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| t == word)
}

/// Pre-flight: ensure enough free disk space (need ~2x the file size).
fn preflight_disk(dir: &Path, need: u64) -> Result<()> {
    // statvfs via /proc is overkill; use `df` output parsing is fragile.
    // Instead: try to get available space via libc statvfs.
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;
        let cpath = CString::new(dir.to_string_lossy().as_bytes()).map_err(|_| {
            Error::Refused("could not check disk space".to_string())
        })?;
        let mut stat: MaybeUninit<libc::statvfs> = MaybeUninit::uninit();
        // SAFETY: statvfs writes to the provided struct.
        let rc = unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) };
        if rc == 0 {
            let stat = unsafe { stat.assume_init() };
            let avail = stat.f_bavail * stat.f_frsize;
            if avail < need.saturating_mul(2) {
                return Err(Error::Refused(format!(
                    "not enough disk space: need ~{} free, have {}",
                    human_size(need * 2),
                    human_size(avail)
                )));
            }
        }
    }
    Ok(())
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// Atomically point `link` at `target` via rename (POSIX atomic).
/// Uses a relative target when it's inside the link's directory, so the
/// link survives the home directory being moved or renamed.
pub fn flip_symlink(link: &Path, target: &Path) -> Result<()> {
    let tmp = link.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let rel = link
        .parent()
        .and_then(|dir| target.strip_prefix(dir).ok())
        .map(|r| r.to_path_buf());
    let link_target = rel.as_deref().unwrap_or(target);
    #[cfg(unix)]
    std::os::unix::fs::symlink(link_target, &tmp)?;
    std::fs::rename(&tmp, link)?;
    Ok(())
}

/// Guess a version from an AppImage filename.
/// `OpenChamber-2.2.0-linux-x86_64.AppImage` -> `Some("2.2.0")`.
/// Returns None when nothing version-like is found.
fn version_from_filename(name: &str) -> Option<String> {
    let stem = name
        .strip_suffix(".AppImage")
        .or_else(|| name.strip_suffix(".appimage"))
        .unwrap_or(name);
    // Find the first run of digits-and-dots that looks like a version.
    let mut best: Option<String> = None;
    let mut cur = String::new();
    let flush = |cur: &mut String, best: &mut Option<String>| {
        if cur.chars().filter(|&c| c == '.').count() >= 1
            && cur.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
            && cur.len() > best.as_ref().map(|b| b.len()).unwrap_or(0)
        {
            *best = Some(cur.clone());
        }
        cur.clear();
    };
    for c in stem.chars() {
        if c.is_ascii_digit() || c == '.' {
            cur.push(c);
        } else {
            flush(&mut cur, &mut best);
        }
    }
    flush(&mut cur, &mut best);
    // Trim trailing dots ("2.0." -> "2.0").
    best.map(|b| b.trim_end_matches('.').to_string())
        .filter(|b| !b.is_empty())
}

struct ExtractedMeta {
    desktop_file: Option<PathBuf>,
    scratch: PathBuf,
}

/// Locate the .desktop file for menu integration.
///
/// Preferred path: read the squashfs payload directly (pure Rust, no
/// execution). Only if that fails (e.g. type-1 AppImages) do we fall back to
/// `<appimage> --appimage-extract`, which executes the AppImage's runtime.
fn extract_metadata(appimage: &Path) -> Result<ExtractedMeta> {
    let scratch = std::env::temp_dir().join(format!(
        "dropin-ai-{}-{}",
        std::process::id(),
        appimage
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)?;
    }
    std::fs::create_dir_all(&scratch)?;

    match extract_via_squashfs(appimage, &scratch) {
        Ok(desktop_file) => {
            // Safe path succeeded: the payload was read without executing
            // anything. Whatever we found (even nothing) is authoritative.
            return Ok(ExtractedMeta {
                desktop_file,
                scratch,
            });
        }
        Err(e) => {
            eprintln!(
                "note: safe metadata extraction failed ({e}); falling back to --appimage-extract"
            );
        }
    }

    let status = std::process::Command::new(appimage)
        .arg("--appimage-extract")
        .current_dir(&scratch)
        .output()
        .map_err(|e| Error::Command(format!("--appimage-extract failed: {e}")))?;
    if !status.status.success() {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(Error::Command(
            "--appimage-extract exited non-zero".into(),
        ));
    }
    let squash = scratch.join("squashfs-root");
    let desktop_file = std::fs::read_dir(&squash)
        .ok()
        .and_then(|entries| {
            entries.flatten().find(|e| {
                e.path().extension().map(|x| x == "desktop").unwrap_or(false)
            })
        })
        .map(|e| e.path())
        .filter(|p| parse_desktop_file(p).is_ok());
    Ok(ExtractedMeta {
        desktop_file,
        scratch,
    })
}

/// Read a type-2 AppImage's embedded .desktop file and icon candidates
/// straight out of its squashfs payload — without executing the AppImage.
///
/// Returns `Ok(None)` when the payload parses but ships no .desktop file.
fn extract_via_squashfs(appimage: &Path, scratch: &Path) -> Result<Option<PathBuf>> {
    use std::io::BufReader;

    let offset = find_squashfs_offset(appimage)?
        .ok_or_else(|| Error::Command("no squashfs payload found".into()))?;
    let reader = BufReader::new(std::fs::File::open(appimage)?);
    let fs = backhand::FilesystemReader::from_reader_with_offset(reader, offset)
        .map_err(|e| Error::Command(format!("squashfs parse failed: {e}")))?;

    let root = scratch.join("squashfs-root");
    std::fs::create_dir_all(&root)?;
    let mut desktop: Option<PathBuf> = None;
    for node in fs.files() {
        let rel = node.fullpath.strip_prefix("/").unwrap_or(&node.fullpath);
        // Top level only, mirroring what --appimage-extract exposes.
        if rel.components().count() != 1 {
            continue;
        }
        // Never write outside the scratch root, whatever the image claims.
        if rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            continue;
        }
        let backhand::InnerNode::File(f) = &node.inner else {
            continue;
        };
        let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_desktop = name.ends_with(".desktop");
        let is_icon = name == ".DirIcon"
            || name.ends_with(".png")
            || name.ends_with(".svg")
            || name.ends_with(".xpm");
        if !(is_desktop || is_icon) {
            continue;
        }
        let dest = root.join(rel);
        let mut out = std::fs::File::create(&dest)?;
        std::io::copy(&mut fs.file(f).reader(), &mut out)?;
        if is_desktop && desktop.is_none() && parse_desktop_file(&dest).is_ok() {
            desktop = Some(dest);
        }
    }
    Ok(desktop)
}

/// Scan a type-2 AppImage for its squashfs payload by magic bytes.
/// Returns the byte offset of the superblock, or `None` if not found
/// (e.g. type-1 AppImages, which use ISO 9660).
fn find_squashfs_offset(appimage: &Path) -> Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom};

    const MAGIC: &[u8] = b"hsqs"; // squashfs superblock magic, little-endian
    const WINDOW: u64 = 1 << 20; // 1 MiB scan windows
    const SUPERBLOCK_LEN: usize = 96;

    let mut f = std::fs::File::open(appimage)?;
    let len = f.metadata()?.len();
    // Overlap windows so the magic (or the superblock) can't straddle one.
    let mut buf = vec![0u8; WINDOW as usize + SUPERBLOCK_LEN];
    let mut pos = 0u64;
    while pos < len {
        f.seek(SeekFrom::Start(pos))?;
        let n = f.read(&mut buf)?;
        if n < MAGIC.len() {
            break;
        }
        let mut start = 0;
        while let Some(i) = buf[start..n].windows(MAGIC.len()).position(|w| w == MAGIC) {
            let at = pos + (start + i) as u64;
            // The magic alone is not enough: it can appear spuriously in the
            // ELF runtime. Validate the superblock before trusting it.
            if superblock_valid(&buf[start + i..]) {
                return Ok(Some(at));
            }
            start += i + 1;
        }
        if n < buf.len() {
            break; // EOF
        }
        pos += WINDOW;
    }
    Ok(None)
}

/// Sanity-check the 96-byte squashfs superblock at the start of `bytes`
/// (which must begin with the magic). Rejects false-positive magic hits in
/// the AppImage runtime.
fn superblock_valid(bytes: &[u8]) -> bool {
    if bytes.len() < 96 || &bytes[0..4] != b"hsqs" {
        return false;
    }
    let u16le = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32le = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);

    let inodes = u32le(4);
    let block_size = u32le(12);
    let compression = u16le(20);
    let block_log = u16le(22);
    let s_major = u16le(28);

    s_major == 4
        && (1..=6).contains(&compression) // gzip, lzma, lzo, xz, lz4, zstd
        && (12..=20).contains(&block_log)
        && block_size == (1u32 << block_log)
        && inodes > 0
        && inodes < 50_000_000
}

#[cfg(test)]
mod safe_extract_tests {
    use super::*;
    use std::io::{Cursor, Seek, Write};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dropin-sqfs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Build a fake type-2 AppImage: junk prefix + real squashfs payload.
    fn make_fake_appimage(dir: &Path) -> PathBuf {
        let mut writer = backhand::FilesystemWriter::default();
        let header = backhand::NodeHeader::new(0o644, 0, 0, 0);
        let desktop = b"[Desktop Entry]\nName=FakeApp\nExec=fakeapp\nType=Application\n";
        writer
            .push_file(Cursor::new(desktop), "/fakeapp.desktop", header)
            .unwrap();
        let mut sq = Cursor::new(Vec::new());
        writer.write(&mut sq).unwrap();
        let sq_bytes = sq.into_inner();

        let appimage = dir.join("fake.AppImage");
        let mut f = std::fs::File::create(&appimage).unwrap();
        // Fake ELF header + padding so the payload sits at a nonzero offset.
        f.write_all(&[0x7f, b'E', b'L', b'F']).unwrap();
        f.write_all(&vec![0u8; 5000]).unwrap();
        f.write_all(&sq_bytes).unwrap();
        appimage
    }

    #[test]
    fn version_from_filename_guesses() {
        assert_eq!(
            version_from_filename("OpenChamber-2.2.0-linux-x86_64.AppImage"),
            Some("2.2.0".to_string())
        );
        assert_eq!(
            version_from_filename("Krita-5.2.1-x86_64.AppImage"),
            Some("5.2.1".to_string())
        );
        assert_eq!(
            version_from_filename("myapp-1.0.AppImage"),
            Some("1.0".to_string())
        );
        assert_eq!(version_from_filename("myapp.AppImage"), None);
        assert_eq!(version_from_filename("myapp-latest.AppImage"), None);
    }

    #[test]
    fn finds_squashfs_payload_offset() {
        let d = tmpdir("offset");
        let ai = make_fake_appimage(&d);
        let off = find_squashfs_offset(&ai).unwrap().expect("offset found");
        assert!(off > 4000, "payload past the padding, got {off}");
        // The magic really is at the reported offset.
        let mut f = std::fs::File::open(&ai).unwrap();
        use std::io::{Read, Seek, SeekFrom};
        f.seek(SeekFrom::Start(off)).unwrap();
        let mut magic = [0u8; 4];
        f.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, b"hsqs");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_squashfs_means_none() {
        let d = tmpdir("none");
        let plain = d.join("plain.bin");
        std::fs::write(&plain, b"just some bytes, no squashfs here").unwrap();
        assert_eq!(find_squashfs_offset(&plain).unwrap(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn spurious_magic_in_prefix_is_skipped() {
        // Regression test: real AppImages (e.g. OpenChamber) can contain the
        // squashfs magic as a false positive inside the ELF runtime. The
        // finder must validate the superblock and keep looking.
        let d = tmpdir("spurious");
        let mut writer = backhand::FilesystemWriter::default();
        let header = backhand::NodeHeader::new(0o644, 0, 0, 0);
        writer
            .push_file(Cursor::new(b"x"), "/f", header)
            .unwrap();
        let mut sq = Cursor::new(Vec::new());
        writer.write(&mut sq).unwrap();
        let sq_bytes = sq.into_inner();

        let appimage = d.join("fake.AppImage");
        let mut f = std::fs::File::create(&appimage).unwrap();
        f.write_all(&[0x7f, b'E', b'L', b'F']).unwrap();
        // Spurious magic + 92 bytes of garbage (not a valid superblock).
        f.write_all(b"hsqs").unwrap();
        f.write_all(&[0xAAu8; 92]).unwrap();
        f.write_all(&[0u8; 1000]).unwrap();
        let real_at = f.stream_position().unwrap();
        f.write_all(&sq_bytes).unwrap();

        let off = find_squashfs_offset(&appimage).unwrap().expect("offset found");
        assert_eq!(off, real_at, "skipped the spurious magic");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn install_uses_generation_layout() {
        let d = tmpdir("gen");
        let home = d.join("home");
        crate::test_env::with_home(&home, || {
            let ai = make_fake_appimage(&d);
            let outcome = install(&ai, &InstallOptions::default()).unwrap();
            let slug = outcome.name;
            let app_dir = home.join("Applications").join(&slug);
            // Version dir exists with the AppImage inside.
            let vers: Vec<_> = std::fs::read_dir(&app_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            assert_eq!(vers.len(), 1, "one generation dir");
            // `active` symlink points at the installed AppImage.
            let active = app_dir.join("active");
            assert!(active.is_symlink(), "active symlink exists");
            let target = std::fs::read_link(&active).unwrap();
            // Relative links resolve against the link's directory.
            let abs_target = if target.is_absolute() {
                target.clone()
            } else {
                active.parent().unwrap().join(&target)
            };
            assert!(abs_target.exists(), "active target exists");
            assert!(target.to_string_lossy().ends_with(".AppImage"));
            // Desktop entry Exec points at `active`.
            let desktop = home.join(format!(
                ".local/share/applications/dropin-{slug}.desktop"
            ));
            let text = std::fs::read_to_string(&desktop).unwrap();
            assert!(
                text.contains(&active.display().to_string()),
                "Exec uses active symlink: {text}"
            );
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn extracts_desktop_without_executing() {
        let d = tmpdir("extract");
        let ai = make_fake_appimage(&d);
        // The "AppImage" is not executable and contains no runtime: if the
        // code tried to run it, this would fail.
        let scratch = d.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let desktop = extract_via_squashfs(&ai, &scratch)
            .unwrap()
            .expect("desktop found");
        let text = std::fs::read_to_string(&desktop).unwrap();
        assert!(text.contains("Name=FakeApp"), "got: {text}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
