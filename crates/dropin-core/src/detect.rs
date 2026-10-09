//! File format detection. Magic bytes first, extension second —
//! never extension alone.

use crate::{clean_name, stem_of, Error, Result};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    AppImage,
    Deb,
    Rpm,
    FlatpakRef,
    FlatpakBundle,
    Script, // .sh / .run / .bin installers
    Tarball,
    Zip,
    WindowsExe, // .exe/.msi -> Wine handoff
    Unknown,
}

impl Format {
    pub fn label(&self) -> &'static str {
        match self {
            Format::AppImage => "AppImage",
            Format::Deb => "Debian package (.deb)",
            Format::Rpm => "RPM package (.rpm)",
            Format::FlatpakRef => "Flatpak ref (.flatpakref)",
            Format::FlatpakBundle => "Flatpak bundle (.flatpak)",
            Format::Script => "installer script",
            Format::Tarball => "compressed archive",
            Format::Zip => "zip archive",
            Format::WindowsExe => "Windows executable",
            Format::Unknown => "unknown",
        }
    }

    /// Can dropin install this without root?
    pub fn user_installable(&self) -> bool {
        matches!(
            self,
            Format::AppImage
                | Format::Deb
                | Format::FlatpakRef
                | Format::FlatpakBundle
                | Format::Tarball
                | Format::Zip
        )
    }
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub format: Format,
    pub name_hint: String,
    pub by_magic: bool,
}

fn read_head(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut f = File::open(path).map_err(|_| Error::Unsupported(path.display().to_string()))?;
    let mut buf = vec![0u8; n];
    let read = f.read(&mut buf)?;
    buf.truncate(read);
    Ok(buf)
}

fn ext(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn is_elf(head: &[u8]) -> bool {
    head.len() >= 4 && head[0] == 0x7f && head[1] == b'E' && head[2] == b'L' && head[3] == b'F'
}

/// AppImage type 2 magic lives at offset 8: 0x41 0x49 0x02 ("AI\x02").
fn is_appimage(head: &[u8]) -> bool {
    is_elf(head) && head.len() >= 11 && head[8] == 0x41 && head[9] == 0x49 && head[10] == 0x02
}

fn looks_like_flatpakref(path: &Path) -> bool {
    read_head(path, 256)
        .map(|b| String::from_utf8_lossy(&b).contains("[Flatpak Ref]"))
        .unwrap_or(false)
}

pub fn detect(path: &Path) -> Result<Detection> {
    if !path.is_file() {
        return Err(Error::Unsupported(format!(
            "not a file: {}",
            path.display()
        )));
    }
    let head = read_head(path, 512)?;
    let extension = ext(path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // --- magic-first checks ---
    if is_appimage(&head) {
        return Ok(mk(path, Format::AppImage, true));
    }
    // ar archive (deb): "!<arch>\n"
    if head.starts_with(b"!<arch>\n") {
        // Peek for debian-binary member to be sure.
        let is_deb = std::fs::read(path)
            .map(|b| b.windows(13).any(|w| w == b"debian-binary"))
            .unwrap_or(false);
        if is_deb || extension == "deb" {
            return Ok(mk(path, Format::Deb, true));
        }
    }
    // rpm: ED AB EE DB
    if head.len() >= 4 && head[0] == 0xED && head[1] == 0xAB && head[2] == 0xEE && head[3] == 0xDB
    {
        return Ok(mk(path, Format::Rpm, true));
    }
    // gzip / xz / bzip2 / zstd -> tarball (extension refines)
    let compressed = (head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b)
        || (head.len() >= 6 && head[0] == 0xFD && head[1] == b'7' && head[2] == b'z')
        || (head.len() >= 3 && head[0] == b'B' && head[1] == b'Z' && head[2] == b'h')
        || (head.len() >= 4 && head[0] == 0x28 && head[1] == 0xB5 && head[2] == 0x2F && head[3] == 0xFD);
    if compressed {
        return Ok(mk(path, Format::Tarball, true));
    }
    // zip: PK\x03\x04 (flatpak bundles are ostree, not zip — handled by extension)
    if head.len() >= 4
        && head[0] == b'P'
        && head[1] == b'K'
        && (extension == "zip" || file_name.ends_with(".zip"))
    {
        return Ok(mk(path, Format::Zip, true));
    }
    // shell script shebang
    if head.starts_with(b"#!") {
        return Ok(mk(path, Format::Script, true));
    }

    // --- extension-driven checks ---
    let fmt = match extension.as_str() {
        "appimage" => Format::AppImage,
        "deb" => Format::Deb,
        "rpm" => Format::Rpm,
        "flatpakref" => {
            if looks_like_flatpakref(path) {
                Format::FlatpakRef
            } else {
                Format::Unknown
            }
        }
        "flatpak" => Format::FlatpakBundle,
        "sh" | "run" | "bin" => Format::Script,
        "gz" | "tgz" | "xz" | "txz" | "bz2" | "tbz2" | "zst" => Format::Tarball,
        "zip" => Format::Zip,
        "exe" | "msi" => Format::WindowsExe,
        _ => {
            // bare "tar" without compression, or extensionless ELF appimage-likes
            if file_name.ends_with(".tar") {
                Format::Tarball
            } else {
                Format::Unknown
            }
        }
    };
    if fmt == Format::Unknown {
        return Err(Error::Unsupported(format!(
            "could not identify {}",
            path.display()
        )));
    }
    Ok(mk(path, fmt, false))
}

fn mk(path: &Path, format: Format, by_magic: bool) -> Detection {
    Detection {
        format,
        name_hint: clean_name(&stem_of(path)),
        by_magic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dropin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn detects_appimage_by_magic() {
        // Fake ELF with AppImage type-2 magic at offset 8.
        let mut b = vec![0x7fu8, b'E', b'L', b'F', 2, 1, 1, 0, 0x41, 0x49, 0x02];
        b.resize(64, 0);
        let p = fixture("weird-name.bin", &b);
        let d = detect(&p).unwrap();
        assert_eq!(d.format, Format::AppImage);
        assert!(d.by_magic);
    }

    #[test]
    fn detects_deb_by_magic() {
        let mut b = b"!<arch>\n".to_vec();
        b.extend_from_slice(b"debian-binary   123456  0     0     0       4         `\n2.0\n");
        let p = fixture("pkg.unknownext", &b);
        let d = detect(&p).unwrap();
        assert_eq!(d.format, Format::Deb);
    }

    #[test]
    fn detects_gzip_tarball() {
        let b = [0x1fu8, 0x8b, 0x08, 0x00];
        let p = fixture("app-1.2.3.tar.gz", &b);
        let d = detect(&p).unwrap();
        assert_eq!(d.format, Format::Tarball);
        assert_eq!(d.name_hint, "app");
    }

    #[test]
    fn detects_script_by_shebang() {
        let p = fixture("installer", b"#!/bin/sh\necho hi\n");
        let d = detect(&p).unwrap();
        assert_eq!(d.format, Format::Script);
        assert!(d.by_magic);
    }

    #[test]
    fn detects_flatpakref_by_content() {
        let p = fixture("app.flatpakref", b"[Flatpak Ref]\nName=org.foo.Bar\n");
        let d = detect(&p).unwrap();
        assert_eq!(d.format, Format::FlatpakRef);
    }

    #[test]
    fn rejects_unknown() {
        let p = fixture("notes.txt", b"hello world");
        assert!(detect(&p).is_err());
    }

    #[test]
    fn name_cleaning() {
        assert_eq!(clean_name("vesktop-1.5.9-x86_64"), "vesktop");
        assert_eq!(clean_name("foo_2.0_amd64"), "foo");
        assert_eq!(clean_name("bar-v3.1-linux"), "bar");
    }
}
