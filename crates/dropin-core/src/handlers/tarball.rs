//! Tarball/zip handler: sniff whether an archive *is an app*, and if so
//! install it rootless into ~/.local/opt/<app>.

use super::{assert_safe_member, sha256_of, InstallOptions, InstallOutcome, InstallPlan};
use crate::desktop::{adopt_desktop_file, generate_entry, parse_desktop_file};
use crate::{detect, slugify, Error, Result, UserDirs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn is_zip(src: &Path) -> bool {
    detect::detect(src)
        .map(|d| d.format == detect::Format::Zip)
        .unwrap_or(false)
}

/// List archive members (one path per line, no leading ./).
/// Pure Rust: no external unzip/tar needed.
fn list_members(src: &Path) -> Result<Vec<String>> {
    let members = if is_zip(src) {
        list_members_zip(src)?
    } else {
        list_members_tar(src)?
    };
    let mut members: Vec<String> = members
        .into_iter()
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

fn list_members_zip(src: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(src).map_err(|e| {
        Error::Command(format!(
            "Couldn't read the zip file. It may have been moved or deleted.\n\nDetails: {e}"
        ))
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| {
        Error::Command(format!(
            "Couldn't read the zip file. It may be corrupted or not actually a zip archive.\n\nDetails: {e}"
        ))
    })?;
    let mut members = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|e| Error::Command(format!("Couldn't read zip entry {i}.\n\nDetails: {e}")))?;
        // Zip paths use forward slashes; the enclosed_name check happens
        // in assert_safe_member via the caller.
        members.push(file.name().to_string());
    }
    Ok(members)
}

/// Detect tar compression from magic bytes (more reliable than extension).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum TarCompression {
    Gzip,
    Bzip2,
    Xz,
    Zstd,
    None,
}

pub(crate) fn detect_tar_compression(src: &Path) -> Result<TarCompression> {
    use std::io::Read;
    let mut f = std::fs::File::open(src).map_err(|e| {
        Error::Command(format!(
            "Couldn't read the archive file. It may have been moved or deleted.\n\nDetails: {e}"
        ))
    })?;
    let mut magic = [0u8; 6];
    let n = f.read(&mut magic).unwrap_or(0);
    if n >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        return Ok(TarCompression::Gzip);
    }
    if n >= 3 && magic[0] == b'B' && magic[1] == b'Z' && magic[2] == b'h' {
        return Ok(TarCompression::Bzip2);
    }
    if n >= 6
        && magic[0] == 0xfd
        && magic[1] == b'7'
        && magic[2] == b'z'
        && magic[3] == b'X'
        && magic[4] == b'Z'
        && magic[5] == 0x00
    {
        return Ok(TarCompression::Xz);
    }
    if n >= 4 && magic[0] == 0x28 && magic[1] == 0xb5 && magic[2] == 0x2f && magic[3] == 0xfd {
        return Ok(TarCompression::Zstd);
    }
    // No compression magic: assume plain tar.
    Ok(TarCompression::None)
}

pub(crate) fn list_members_tar(src: &Path) -> Result<Vec<String>> {
    let comp = detect_tar_compression(src)?;
    let file = std::fs::File::open(src)
        .map_err(|e| Error::Command(format!("Couldn't read the archive file.\n\nDetails: {e}")))?;
    let mut members = Vec::new();
    match comp {
        TarCompression::Gzip => {
            let gz = flate2::read::GzDecoder::new(file);
            collect_tar_members(gz, &mut members)?;
        }
        TarCompression::Bzip2 => {
            let bz = bzip2::read::BzDecoder::new(file);
            collect_tar_members(bz, &mut members)?;
        }
        TarCompression::Xz => {
            let xz = liblzma::read::XzDecoder::new(file);
            collect_tar_members(xz, &mut members)?;
        }
        TarCompression::Zstd => {
            let zst = zstd::Decoder::new(file).map_err(|e| {
                Error::Command(format!("Couldn't decompress zstd archive.\n\nDetails: {e}"))
            })?;
            collect_tar_members(zst, &mut members)?;
        }
        TarCompression::None => {
            collect_tar_members(file, &mut members)?;
        }
    }
    Ok(members)
}

fn collect_tar_members<R: std::io::Read>(reader: R, members: &mut Vec<String>) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|e| {
        Error::Command(format!(
            "Couldn't read the tar archive. It may be corrupted.\n\nDetails: {e}"
        ))
    })? {
        let entry = entry.map_err(|e| {
            Error::Command(format!(
                "Couldn't read a tar entry. The archive may be corrupted.\n\nDetails: {e}"
            ))
        })?;
        let path = entry.path().map_err(|e| {
            Error::Command(format!("Couldn't read a tar entry path.\n\nDetails: {e}"))
        })?;
        members.push(path.to_string_lossy().to_string());
    }
    Ok(())
}

fn extract(src: &Path, dest: &Path, strip: usize) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    if is_zip(src) {
        extract_zip(src, dest, strip)?;
    } else {
        extract_tar(src, dest, strip)?;
    }
    Ok(())
}

/// Extract a zip file in pure Rust (handles zstd, bzip2, lzma natively).
/// Applies strip-components by skipping leading path components.
fn extract_zip(src: &Path, dest: &Path, strip: usize) -> Result<()> {
    let file = std::fs::File::open(src).map_err(|e| {
        Error::Command(format!(
            "Couldn't read the zip file. It may have been moved or deleted.\n\nDetails: {e}"
        ))
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| {
        Error::Command(format!(
            "Couldn't extract the zip file. It may be corrupted or incomplete — try downloading it again.\n\nDetails: {e}"
        ))
    })?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| {
            Error::Command(format!(
                "Couldn't extract the zip file (entry {i}).\n\nDetails: {e}"
            ))
        })?;
        let name = file.name().to_string();
        // Skip directory entries; we create parents as needed.
        if name.ends_with('/') {
            continue;
        }
        // Apply strip: skip entries that don't have enough components.
        let stripped = strip_path(&name, strip);
        let Some(rel) = stripped else {
            continue;
        };
        // Safety: assert_safe_member already ran on list_members, but
        // double-check here since extract can be called directly.
        assert_safe_member(&rel)?;
        let out_path = dest.join(&rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut file, &mut out)?;
        // Preserve Unix permissions if present.
        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

/// Strip leading path components (like tar --strip-components).
/// Returns None if the path has fewer components than strip.
fn strip_path(path: &str, strip: usize) -> Option<String> {
    if strip == 0 {
        return Some(path.to_string());
    }
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    for _ in 0..strip {
        parts.next()?;
    }
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() {
        return None;
    }
    Some(rest.join("/"))
}

pub(crate) fn extract_tar(src: &Path, dest: &Path, strip: usize) -> Result<()> {
    let comp = detect_tar_compression(src)?;
    let file = std::fs::File::open(src)
        .map_err(|e| Error::Command(format!("Couldn't read the archive file.\n\nDetails: {e}")))?;
    match comp {
        TarCompression::Gzip => {
            let gz = flate2::read::GzDecoder::new(file);
            unpack_tar(gz, dest, strip)?;
        }
        TarCompression::Bzip2 => {
            let bz = bzip2::read::BzDecoder::new(file);
            unpack_tar(bz, dest, strip)?;
        }
        TarCompression::Xz => {
            let xz = liblzma::read::XzDecoder::new(file);
            unpack_tar(xz, dest, strip)?;
        }
        TarCompression::Zstd => {
            let zst = zstd::Decoder::new(file).map_err(|e| {
                Error::Command(format!("Couldn't decompress zstd archive.\n\nDetails: {e}"))
            })?;
            unpack_tar(zst, dest, strip)?;
        }
        TarCompression::None => {
            unpack_tar(file, dest, strip)?;
        }
    }
    Ok(())
}

fn unpack_tar<R: std::io::Read>(reader: R, dest: &Path, strip: usize) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    // We extract to a temp dir first, then move_stripped handles the
    // strip-components logic (same as the old zip path).
    let tmp = dest.join(".dropin-tar-tmp");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    std::fs::create_dir_all(&tmp)?;
    let result = (|| -> Result<()> {
        for entry in archive.entries().map_err(|e| {
            Error::Command(format!(
                "Couldn't read the tar archive. It may be corrupted.\n\nDetails: {e}"
            ))
        })? {
            let mut entry = entry.map_err(|e| {
                Error::Command(format!("Couldn't extract the tar archive.\n\nDetails: {e}"))
            })?;
            // Security: the tar crate's unpack_in checks for traversal,
            // but we validate explicitly for a clear error message.
            let path = entry.path().map_err(|e| {
                Error::Command(format!("Couldn't read a tar entry path.\n\nDetails: {e}"))
            })?;
            let rel = path.to_string_lossy().to_string();
            assert_safe_member(&rel)?;
            entry.unpack_in(&tmp).map_err(|e| {
                Error::Command(format!("Couldn't extract the tar archive.\n\nDetails: {e}"))
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        return result;
    }
    move_stripped(&tmp, dest, strip)?;
    std::fs::remove_dir_all(&tmp)?;
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

/// Cache of extracted archives: plan() extracts once, install() reuses.
/// Key: (canonical source path, mtime nanos, file size).
/// Value: (temp dir, insertion time).
static EXTRACTION_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(PathBuf, u64, u64), (PathBuf, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Max cached extractions; evict oldest beyond this.
const CACHE_LIMIT: usize = 8;
/// Evict cache entries older than this.
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Insert into the extraction cache, evicting stale/old entries.
fn cache_insert(id: (PathBuf, u64, u64), dir: PathBuf) {
    let Ok(mut cache) = EXTRACTION_CACHE.lock() else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let now = std::time::Instant::now();
    // Evict expired.
    cache.retain(|_, (_, t)| now.duration_since(*t) < CACHE_TTL);
    // Evict oldest if over limit.
    while cache.len() >= CACHE_LIMIT {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (_, t))| *t)
            .map(|(k, _)| k.clone())
        {
            if let Some((dir, _)) = cache.remove(&oldest) {
                let _ = std::fs::remove_dir_all(&dir);
            }
        } else {
            break;
        }
    }
    cache.insert(id, (dir, now));
}

/// Take a cached extraction (removing it from the cache).
fn cache_take(id: &(PathBuf, u64, u64)) -> Option<PathBuf> {
    let mut cache = EXTRACTION_CACHE.lock().ok()?;
    let (dir, _) = cache.remove(id)?;
    // Clean up expired entries opportunistically.
    let now = std::time::Instant::now();
    let stale: Vec<_> = cache
        .iter()
        .filter(|(_, (_, t))| now.duration_since(*t) >= CACHE_TTL)
        .map(|(k, _)| k.clone())
        .collect();
    for k in stale {
        if let Some((d, _)) = cache.remove(&k) {
            let _ = std::fs::remove_dir_all(&d);
        }
    }
    if dir.exists() {
        Some(dir)
    } else {
        None
    }
}

/// Identify a source file for cache lookup.
fn source_id(src: &Path) -> Result<(PathBuf, u64, u64)> {
    let canonical = src
        .canonicalize()
        .map_err(|e| Error::Command(format!("Couldn't read the archive file.\n\nDetails: {e}")))?;
    let meta = std::fs::metadata(&canonical)
        .map_err(|e| Error::Command(format!("Couldn't read the archive file.\n\nDetails: {e}")))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Ok((canonical, mtime, meta.len()))
}

/// Extract to a temp dir and return its path (caller must clean up on error).
fn extract_to_temp(src: &Path, strip: usize) -> Result<PathBuf> {
    let tmp = crate::unique_temp_dir("dropin-extract");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    extract(src, &tmp, strip)?;
    Ok(tmp)
}

/// Inspect an already-extracted directory for app indicators.
fn sniff_dir(dir: &Path) -> Sniff {
    let mut desktop_files = Vec::new();
    let mut executables = Vec::new();
    let mut has_app_asar = false;
    for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
        let rel = entry
            .path()
            .strip_prefix(dir)
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
    Sniff {
        desktop_files,
        executables,
        has_app_asar,
    }
}

fn sniff(_members: &[String], src: &Path, strip: usize) -> Result<(Sniff, PathBuf)> {
    // Extract once to a temp dir, sniff from there.
    // The caller owns the temp dir: plan() caches it, install() reuses or cleans up.
    let tmp = extract_to_temp(src, strip)?;
    let sniff = sniff_dir(&tmp);
    Ok((sniff, tmp))
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
    let (sniff, tmp) = sniff(&members, src, strip)?;
    if !looks_like_app(&sniff) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(Error::Refused(format!(
            "{} looks like data, not an application (no executables or .desktop files found); opening with your archive manager instead",
            src.display()
        )));
    }
    // Cache the extraction for install() to reuse (avoids extracting twice).
    if let Ok(id) = source_id(src) {
        cache_insert(id, tmp);
    } else {
        let _ = std::fs::remove_dir_all(&tmp);
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

    // Reuse the extraction from plan() if available (avoids extracting twice).
    // Otherwise extract fresh.
    let (sniff, tmp): (Sniff, Option<PathBuf>) =
        match source_id(src).ok().and_then(|id| cache_take(&id)) {
            Some(cached) => (sniff_dir(&cached), Some(cached)),
            None => {
                let (s, t) = sniff(&members, src, strip)?;
                (s, Some(t))
            }
        };
    // If we extracted fresh here (not from plan's cache), clean up on error.
    // The success path moves the temp dir to the final location.
    let tmp_dir = tmp.unwrap(); // sniff always returns a temp dir
    let result = (|| -> Result<InstallOutcome> {
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

        // Move the extracted temp dir to the final location (fast rename,
        // no re-extraction).
        std::fs::rename(&tmp_dir, &root)?;
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
    })();
    // Clean up temp dir if we failed (success moved it via rename).
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a zip in memory with the given compression, return the bytes.
    fn make_zip(compression: zip::CompressionMethod) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(compression)
                .unix_permissions(0o755);
            w.start_file("myapp/run.sh", opts).unwrap();
            w.write_all(b"#!/bin/sh\necho hi\n").unwrap();
            w.start_file("myapp/readme.txt", opts).unwrap();
            w.write_all(b"hello\n").unwrap();
            w.finish().unwrap();
        }
        buf
    }

    fn write_temp(name: &str, data: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dropin-test-{name}"));
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn zip_deflate_lists_and_extracts() {
        let p = write_temp("deflate.zip", &make_zip(zip::CompressionMethod::Deflated));
        let members = list_members(&p).unwrap();
        assert!(members.contains(&"myapp/run.sh".to_string()));
        assert!(members.contains(&"myapp/readme.txt".to_string()));

        let dest = std::env::temp_dir().join("dropin-test-deflate-out");
        let _ = std::fs::remove_dir_all(&dest);
        extract(&p, &dest, 1).unwrap();
        // strip=1 removes the myapp/ prefix
        assert!(dest.join("run.sh").exists());
        assert!(dest.join("readme.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("readme.txt")).unwrap(),
            "hello\n"
        );
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn zip_zstd_lists_and_extracts() {
        // zstd (method 93) is what broke `unzip` for the user's game archive.
        let p = write_temp("zstd.zip", &make_zip(zip::CompressionMethod::Zstd));
        let members = list_members(&p).unwrap();
        assert!(members.contains(&"myapp/run.sh".to_string()));

        let dest = std::env::temp_dir().join("dropin-test-zstd-out");
        let _ = std::fs::remove_dir_all(&dest);
        extract(&p, &dest, 1).unwrap();
        assert!(dest.join("run.sh").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("readme.txt")).unwrap(),
            "hello\n"
        );
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn zip_bzip2_lists_and_extracts() {
        let p = write_temp("bzip2.zip", &make_zip(zip::CompressionMethod::Bzip2));
        let members = list_members(&p).unwrap();
        assert!(members.contains(&"myapp/run.sh".to_string()));

        let dest = std::env::temp_dir().join("dropin-test-bzip2-out");
        let _ = std::fs::remove_dir_all(&dest);
        extract(&p, &dest, 0).unwrap();
        assert!(dest.join("myapp/run.sh").exists());
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn zip_stored_lists_and_extracts() {
        let p = write_temp("stored.zip", &make_zip(zip::CompressionMethod::Stored));
        let members = list_members(&p).unwrap();
        assert_eq!(members.len(), 2);
        let dest = std::env::temp_dir().join("dropin-test-stored-out");
        let _ = std::fs::remove_dir_all(&dest);
        extract(&p, &dest, 0).unwrap();
        assert!(dest.join("myapp/readme.txt").exists());
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn zip_traversal_blocked() {
        // A zip with ../ should be refused, not extracted outside dest.
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("../../../evil.txt", opts).unwrap();
            w.write_all(b"evil\n").unwrap();
            w.finish().unwrap();
        }
        let p = write_temp("traversal.zip", &buf);
        // list_members must reject it
        assert!(list_members(&p).is_err());
        let dest = std::env::temp_dir().join("dropin-test-traversal-out");
        let _ = std::fs::remove_dir_all(&dest);
        assert!(extract(&p, &dest, 0).is_err());
        // And nothing escaped
        assert!(!std::env::temp_dir().join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn strip_path_works() {
        assert_eq!(strip_path("a/b/c.txt", 0), Some("a/b/c.txt".into()));
        assert_eq!(strip_path("a/b/c.txt", 1), Some("b/c.txt".into()));
        assert_eq!(strip_path("a/b/c.txt", 2), Some("c.txt".into()));
        assert_eq!(strip_path("a/b/c.txt", 3), None);
        assert_eq!(strip_path("a/b/c.txt", 5), None);
    }
}
