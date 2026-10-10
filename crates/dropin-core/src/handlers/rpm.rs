//! .rpm handler: a minimal pure-Rust RPM reader (no rpm2cpio needed),
//! then a rootless install mirroring the .deb handler.
//!
//! Format reference (rpm.org, "RPM file format"):
//!   lead (96 bytes) | signature header | header | payload (cpio, compressed)
//! Each header: magic 8E AD E8, version, 4 reserved bytes, index count (u32),
//! data length (u32), index entries (16 bytes each), data store (padded to 8).

use super::{assert_safe_member, sha256_of, InstallOptions, InstallOutcome, InstallPlan};
use crate::desktop::{adopt_desktop_file, parse_desktop_file};
use crate::{have, slugify, Error, Result, UserDirs};
use flate2::read::GzDecoder;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

// Header index tags we care about.
const TAG_NAME: u32 = 1000;
const TAG_VERSION: u32 = 1001;
const TAG_PAYLOADCOMPRESSOR: u32 = 1125;
// Index data types.
const TYPE_STRING: u32 = 6;

#[derive(Debug)]
struct IndexEntry {
    tag: u32,
    typ: u32,
    offset: u32,
    #[allow(dead_code)]
    count: u32,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn pad8(n: usize) -> usize {
    (n + 7) & !7
}

/// Parse one header at `pos`. Returns (entries, data_store_start, header_end).
fn parse_header(data: &[u8], pos: usize) -> Result<(Vec<IndexEntry>, usize, usize)> {
    if data.len() < pos + 16 || data[pos..pos + 3] != [0x8E, 0xAD, 0xE8] {
        return Err(Error::Unsupported("bad RPM header magic".into()));
    }
    let count = be32(&data[pos + 8..pos + 12]) as usize;
    let data_len = be32(&data[pos + 12..pos + 16]) as usize;
    let entries_start = pos + 16;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let e = entries_start + i * 16;
        if data.len() < e + 16 {
            return Err(Error::Unsupported("truncated RPM header".into()));
        }
        entries.push(IndexEntry {
            tag: be32(&data[e..e + 4]),
            typ: be32(&data[e + 4..e + 8]),
            offset: be32(&data[e + 8..e + 12]),
            count: be32(&data[e + 12..e + 16]),
        });
    }
    let store = entries_start + count * 16;
    let end = store + pad8(data_len);
    if data.len() < end {
        return Err(Error::Unsupported("truncated RPM header data".into()));
    }
    Ok((entries, store, end))
}

fn header_string(data: &[u8], entries: &[IndexEntry], store: usize, tag: u32) -> Option<String> {
    let e = entries
        .iter()
        .find(|e| e.tag == tag && e.typ == TYPE_STRING)?;
    let start = store + e.offset as usize;
    let bytes = data.get(start..)?;
    let len = bytes.iter().position(|&b| b == 0)?;
    String::from_utf8(bytes[..len].to_vec()).ok()
}

struct RpmMeta {
    name: String,
    version: Option<String>,
    compressor: String,
    payload_offset: usize,
}

fn read_meta(path: &Path) -> Result<RpmMeta> {
    let data = std::fs::read(path)?;
    if data.len() < 96 || data[0..4] != [0xED, 0xAB, 0xEE, 0xDB] {
        return Err(Error::Unsupported("not an RPM (bad lead magic)".into()));
    }
    let mut pos = 96usize;
    // Skip the signature header; we only need its extent.
    let (_, _, sig_end) = parse_header(&data, pos)?;
    pos = sig_end;
    let (entries, store, hdr_end) = parse_header(&data, pos)?;
    let name = header_string(&data, &entries, store, TAG_NAME)
        .ok_or_else(|| Error::Unsupported("RPM has no name".into()))?;
    Ok(RpmMeta {
        name,
        version: header_string(&data, &entries, store, TAG_VERSION),
        compressor: header_string(&data, &entries, store, TAG_PAYLOADCOMPRESSOR)
            .unwrap_or_else(|| "gzip".to_string()),
        payload_offset: hdr_end,
    })
}

/// Decompress the payload according to RPMTAG_PAYLOADCOMPRESSOR.
fn payload_bytes(path: &Path, meta: &RpmMeta) -> Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    let raw = data
        .get(meta.payload_offset..)
        .ok_or_else(|| Error::Unsupported("RPM payload missing".into()))?;
    match meta.compressor.as_str() {
        "gzip" => {
            let mut dec = GzDecoder::new(raw);
            let mut out = Vec::new();
            dec.read_to_end(&mut out)?;
            Ok(out)
        }
        "none" | "" => Ok(raw.to_vec()),
        other @ ("xz" | "zstd") => {
            let tool = if other == "xz" { "unxz" } else { "unzstd" };
            if !have(tool) {
                return Err(Error::Command(format!(
                    "this RPM uses {other} compression but `{tool}` is not installed"
                )));
            }
            let tmp = std::env::temp_dir().join(format!("dropin-rpmpay-{}", std::process::id()));
            std::fs::write(&tmp, raw)?;
            let out = std::process::Command::new(tool)
                .args(["-c", tmp.to_str().unwrap()])
                .output()
                .map_err(|e| Error::Command(format!("{tool} failed: {e}")))?;
            let _ = std::fs::remove_file(&tmp);
            if !out.status.success() {
                return Err(Error::Command(format!(
                    "{tool} could not decompress payload"
                )));
            }
            Ok(out.stdout)
        }
        other => Err(Error::Unsupported(format!(
            "unsupported RPM payload compressor: {other}"
        ))),
    }
}

fn hex8(b: &[u8]) -> Result<u32> {
    let s = std::str::from_utf8(b).map_err(|_| Error::Unsupported("bad cpio header".into()))?;
    u32::from_str_radix(s.trim(), 16).map_err(|_| Error::Unsupported("bad cpio header".into()))
}

/// Extract a cpio "newc" (070701) archive into `dest`.
fn extract_cpio(payload: &[u8], dest: &Path) -> Result<()> {
    let mut pos = 0usize;
    loop {
        if pos + 110 > payload.len() {
            return Err(Error::Unsupported("truncated cpio archive".into()));
        }
        let h = &payload[pos..pos + 110];
        if &h[0..6] != b"070701" {
            return Err(Error::Unsupported("not a newc cpio archive".into()));
        }
        let mode = hex8(&h[14..22])?;
        let filesize = hex8(&h[54..62])? as usize;
        let namesize = hex8(&h[94..102])? as usize;
        pos += 110;
        if pos + namesize > payload.len() {
            return Err(Error::Unsupported("truncated cpio names".into()));
        }
        let name_bytes = &payload[pos..pos + namesize];
        let name = std::str::from_utf8(&name_bytes[..namesize.saturating_sub(1)])
            .map_err(|_| Error::Unsupported("bad cpio filename".into()))?;
        pos += (namesize + 3) & !3;
        if name == "TRAILER!!!" {
            break;
        }
        assert_safe_member(name)?;
        let target = dest.join(name);
        let file_type = mode & 0o170000;
        if file_type == 0o040000 {
            std::fs::create_dir_all(&target)?;
        } else if file_type == 0o120000 {
            // Symlink: data is the target path.
            if pos + filesize > payload.len() {
                return Err(Error::Unsupported("truncated cpio data".into()));
            }
            let link_target = std::str::from_utf8(&payload[pos..pos + filesize]).unwrap_or("");
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if target.exists() || target.is_symlink() {
                std::fs::remove_file(&target)?;
            }
            std::os::unix::fs::symlink(link_target, &target)?;
        } else if file_type == 0o100000 {
            if pos + filesize > payload.len() {
                return Err(Error::Unsupported("truncated cpio data".into()));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, &payload[pos..pos + filesize])?;
            let mut perms = std::fs::metadata(&target)?.permissions();
            perms.set_mode(mode & 0o7777);
            std::fs::set_permissions(&target, perms)?;
        }
        // Other types (devices, fifos) are skipped: not meaningful rootless.
        pos += (filesize + 3) & !3;
    }
    Ok(())
}

pub fn plan(src: &Path) -> Result<InstallPlan> {
    let meta = read_meta(src)?;
    Ok(InstallPlan {
        app_name: slugify(&meta.name),
        version: meta.version.clone(),
        steps: vec![
            format!(
                "Extract package payload to ~/.local/opt/{} (no root needed)",
                slugify(&meta.name)
            ),
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
    let meta = read_meta(src)?;
    let pkg = slugify(&meta.name);
    let dirs = UserDirs::new();
    dirs.ensure()?;
    let root = dirs.opt.join(&pkg);
    if root.exists() {
        return Err(Error::Refused(format!(
            "~/.local/opt/{pkg} already exists — remove it first with `dropin remove {pkg}`"
        )));
    }
    std::fs::create_dir_all(&root)?;
    let payload = payload_bytes(src, &meta)?;
    if let Err(e) = extract_cpio(&payload, &root) {
        let _ = std::fs::remove_dir_all(&root);
        return Err(e);
    }

    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&root)
        .into_iter()
        .flatten()
        .map(|e| e.path().to_path_buf())
        .collect();

    // Adopt bundled .desktop files.
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

    // Symlink usr/bin/* into ~/.local/bin.
    for bindir in ["usr/bin", "usr/local/bin", "bin"] {
        let dir = root.join(bindir);
        if !dir.is_dir() {
            continue;
        }
        let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|r| r.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        for bin in entries {
            if bin.is_file() {
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
        version: meta.version,
        install_root: Some(root),
        paths,
        external: false,
        update_info: None,
    })
}

fn install_system(src: &Path) -> Result<InstallOutcome> {
    let s = src
        .to_str()
        .ok_or_else(|| Error::Unsupported("bad path".into()))?;
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let (cmd, args): (&str, Vec<&str>) = if os_release.contains("fedora")
        || os_release.contains("rhel")
    {
        ("pkexec", vec!["dnf", "install", "-y", s])
    } else if os_release.contains("arch") {
        ("pkexec", vec!["pacman", "-U", "--noconfirm", s])
    } else if os_release.contains("suse") || os_release.contains("opensuse") {
        ("pkexec", vec!["zypper", "--non-interactive", "install", s])
    } else {
        return Err(Error::Refused(
            "no supported system package manager detected for --system install of an rpm".into(),
        ));
    };
    let status = std::process::Command::new(cmd)
        .args(&args)
        .status()
        .map_err(|e| Error::Command(format!("system install failed: {e}")))?;
    if !status.success() {
        return Err(Error::Command("system package install failed".into()));
    }
    let pkg = slugify(&read_meta(src).map(|m| m.name).unwrap_or_default());
    Ok(InstallOutcome {
        name: pkg,
        version: None,
        install_root: None,
        paths: vec![],
        external: true,
        update_info: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::InstallOptions;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn u32be(v: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    /// Minimal but spec-shaped RPM: lead + empty sig header + header with
    /// NAME/VERSION/PAYLOADCOMPRESSOR + gzip'd cpio payload.
    fn build_fixture(dir: &Path) -> PathBuf {
        // --- cpio newc payload ---
        let mut cpio = Vec::new();
        let mut entry = |name: &str, mode: u32, data: &[u8]| {
            let mut h = String::from("070701");
            let fields: [u32; 13] = [
                1,
                mode,
                0,
                0,
                1,
                0,
                data.len() as u32,
                0,
                0,
                0,
                0,
                name.len() as u32 + 1,
                0,
            ];
            for f in fields {
                h.push_str(&format!("{f:08x}"));
            }
            cpio.extend_from_slice(h.as_bytes());
            cpio.extend_from_slice(name.as_bytes());
            cpio.push(0);
            let namepad = (4 - (name.len() + 1) % 4) % 4;
            cpio.extend(std::iter::repeat_n(0, namepad));
            cpio.extend_from_slice(data);
            let datapad = (4 - data.len() % 4) % 4;
            cpio.extend(std::iter::repeat_n(0, datapad));
        };
        entry("usr", 0o040755, &[]);
        entry("usr/bin", 0o040755, &[]);
        entry(
            "usr/bin/demotool2",
            0o100755,
            b"#!/bin/sh\necho demotool2\n",
        );
        entry("usr/share", 0o040755, &[]);
        entry("usr/share/applications", 0o040755, &[]);
        entry(
            "usr/share/applications/demotool2.desktop",
            0o100644,
            b"[Desktop Entry]\nType=Application\nName=DemoTool2\nExec=/usr/bin/demotool2\n",
        );
        entry("TRAILER!!!", 0, &[]);

        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&cpio).unwrap();
        let payload = enc.finish().unwrap();

        // --- header builder ---
        fn header(entries: &[(u32, u32, u32, u32)], store: &[u8]) -> Vec<u8> {
            let mut h = vec![0x8E, 0xAD, 0xE8, 1, 0, 0, 0, 0];
            u32be(entries.len() as u32, &mut h);
            u32be(store.len() as u32, &mut h);
            for (tag, typ, off, cnt) in entries {
                u32be(*tag, &mut h);
                u32be(*typ, &mut h);
                u32be(*off, &mut h);
                u32be(*cnt, &mut h);
            }
            h.extend_from_slice(store);
            while h.len() % 8 != 0 {
                h.push(0);
            }
            h
        }
        let name = b"demotool2\0";
        let version = b"1.0\0";
        let comp = b"gzip\0";
        let mut store = Vec::new();
        store.extend_from_slice(name);
        store.extend_from_slice(version);
        store.extend_from_slice(comp);
        let main = header(
            &[
                (TAG_NAME, TYPE_STRING, 0, 1),
                (TAG_VERSION, TYPE_STRING, name.len() as u32, 1),
                (
                    TAG_PAYLOADCOMPRESSOR,
                    TYPE_STRING,
                    (name.len() + version.len()) as u32,
                    1,
                ),
            ],
            &store,
        );
        let sig = header(&[], &[]);

        // --- lead (96 bytes) ---
        let mut rpm = vec![0xED, 0xAB, 0xEE, 0xDB, 3, 0];
        rpm.extend_from_slice(&0u16.to_be_bytes()); // type
        rpm.extend_from_slice(&1u16.to_be_bytes()); // archnum
        let mut name66 = [0u8; 66];
        name66[..9].copy_from_slice(b"demotool2");
        rpm.extend_from_slice(&name66);
        rpm.extend_from_slice(&1u16.to_be_bytes()); // osnum
        rpm.extend_from_slice(&5u16.to_be_bytes()); // signature type
        rpm.extend_from_slice(&[0u8; 16]);
        assert_eq!(rpm.len(), 96);

        rpm.extend_from_slice(&sig);
        rpm.extend_from_slice(&main);
        rpm.extend_from_slice(&payload);

        let path = dir.join("demotool2-1.0.x86_64.rpm");
        std::fs::write(&path, &rpm).unwrap();
        path
    }

    #[test]
    fn reads_meta_from_spec_built_rpm() {
        let dir = std::env::temp_dir().join(format!("dropin-rpm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rpm = build_fixture(&dir);
        let meta = read_meta(&rpm).unwrap();
        assert_eq!(meta.name, "demotool2");
        assert_eq!(meta.version.as_deref(), Some("1.0"));
        assert_eq!(meta.compressor, "gzip");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_cpio_payload() {
        let dir = std::env::temp_dir().join(format!("dropin-rpmx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rpm = build_fixture(&dir);
        let meta = read_meta(&rpm).unwrap();
        let payload = payload_bytes(&rpm, &meta).unwrap();
        let dest = dir.join("out");
        std::fs::create_dir_all(&dest).unwrap();
        extract_cpio(&payload, &dest).unwrap();
        let bin = dest.join("usr/bin/demotool2");
        assert!(bin.exists());
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert!(dest
            .join("usr/share/applications/demotool2.desktop")
            .exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_remove_roundtrip() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("dropin-rpmi-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join("home");
        crate::test_env::with_home(&home, || {
            let rpm = build_fixture(&dir);
            let outcome = crate::install::install(&rpm, &InstallOptions::default()).unwrap();
            assert_eq!(outcome.name, "demotool2");
            assert!(home.join(".local/bin/demotool2").is_symlink());
            assert!(home
                .join(".local/share/applications/dropin-demotool2.desktop")
                .exists());
            crate::install::remove("demotool2", false).unwrap();
            assert!(!home.join(".local/opt/demotool2").exists());
            assert!(!home.join(".local/bin/demotool2").exists());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
