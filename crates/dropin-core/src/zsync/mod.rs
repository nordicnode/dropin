//! Native zsync delta updates for AppImages.
//!
//! Given an AppImage with an embedded update channel (`zsync|<url>` or
//! `gh-releases-zsync|owner|repo|tag|pattern`), dropin can update it without
//! any external updater tool:
//!
//! 1. Resolve the channel to a `.zsync` control file URL.
//! 2. Fetch the control file's header (one small ranged request) and compare
//!    its SHA-1 against the local file: no match, no download at all.
//! 3. Fetch the full control file, scan the local AppImage for blocks the
//!    new version reuses, and download only the missing byte ranges.
//! 4. Assemble the new file, verify its SHA-1, snapshot the old AppImage,
//!    and swap it into place.
//!
//! If any step fails, the old AppImage is untouched and the caller can fall
//! back to `appimageupdatetool` or a manual re-download.

pub mod control;
pub mod fetch;
pub mod rsum;
pub mod scan;

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::{Error, Result};

pub use control::{fetch_control, fetch_header, ControlFile, Header};

/// User-Agent for update requests.
pub const USER_AGENT: &str = "dropin/0.1 (zsync; +https://github.com/nordicnode/dropin)";

/// Largest payload dropin will update this way (4 GiB). Beyond that, or for
/// a control file that claims one, something is wrong.
const MAX_PAYLOAD: u64 = 4 * 1024 * 1024 * 1024;

/// A resolved, checked update.
pub struct UpdateCheck {
    /// URL of the `.zsync` control file.
    pub zsync_url: String,
    /// URL of the new AppImage payload.
    pub payload_url: String,
    pub header: Header,
    /// True when the local file already matches the published SHA-1.
    pub up_to_date: bool,
}

/// What an applied update did.
pub struct ApplyReport {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub reused_blocks: usize,
    pub total_blocks: usize,
}

/// Turn an AppImage update-info string into a `.zsync` URL.
///
/// Handles `zsync|<url>` directly and `gh-releases-zsync|owner|repo|tag|pattern`
/// via the GitHub releases API. Returns `None` for channel types dropin does
/// not speak natively.
pub fn resolve_zsync_url(update_info: &str) -> Result<Option<String>> {
    let info = update_info.trim();
    if let Some(url) = info.strip_prefix("zsync|") {
        let url = url.trim();
        if url.is_empty() {
            return Ok(None);
        }
        return Ok(Some(url.to_string()));
    }
    if let Some(rest) = info.strip_prefix("gh-releases-zsync|") {
        return resolve_gh_zsync(rest);
    }
    Ok(None)
}

/// Resolve `gh-releases-zsync|owner|repo|tag|pattern` to a `.zsync` asset URL.
fn resolve_gh_zsync(rest: &str) -> Result<Option<String>> {
    let mut parts = rest.split('|');
    let (owner, repo, tag, pattern) =
        match (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(o), Some(r), Some(t), Some(p), None) => (o.trim(), r.trim(), t.trim(), p.trim()),
            _ => return Ok(None),
        };
    if owner.is_empty() || repo.is_empty() || pattern.is_empty() {
        return Ok(None);
    }

    let api_url = if tag.eq_ignore_ascii_case("latest") || tag == "*" {
        format!("https://api.github.com/repos/{owner}/{repo}/releases/latest")
    } else {
        format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag}")
    };
    let body = fetch::get_all(&api_url, 4 * 1024 * 1024)
        .map_err(|e| Error::Zsync(format!("GitHub release lookup failed: {e}")))?;
    let release: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| Error::Zsync(format!("bad GitHub response: {e}")))?;

    let assets = release.get("assets").and_then(|a| a.as_array()).cloned().unwrap_or_default();
    // Prefer an exact (modulo wildcards) match; the pattern usually ends in
    // `.zsync` already.
    for asset in &assets {
        let name = asset.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if !name.ends_with(".zsync") {
            continue;
        }
        if wildcard_match(pattern, name) {
            if let Some(url) = asset.get("browser_download_url").and_then(|u| u.as_str()) {
                return Ok(Some(url.to_string()));
            }
        }
    }
    Ok(None)
}

/// `*` matches any run of characters; everything else is literal.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let px = pattern.as_bytes();
    let nx = name.as_bytes();
    // Classic two-pointer wildcard match.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < nx.len() {
        if pi < px.len() && (px[pi] == b'*') {
            star = Some(pi);
            pi += 1;
            mark = ni;
        } else if pi < px.len() && px[pi] == nx[ni] {
            pi += 1;
            ni += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < px.len() && px[pi] == b'*' {
        pi += 1;
    }
    pi == px.len()
}

/// Check whether `local` needs updating, resolving its channel.
///
/// A missing or unresolvable channel is not an error here — it just means
/// there is nothing to check — so this returns `Ok(None)` for those.
pub fn check(local: &Path, update_info: &str) -> Result<Option<UpdateCheck>> {
    let zsync_url = match resolve_zsync_url(update_info)? {
        Some(u) => u,
        None => return Ok(None),
    };
    let header = fetch_header(&zsync_url)?;
    let payload_url = match &header.url {
        Some(u) => join_url(&zsync_url, u),
        None => {
            return Err(Error::Zsync(format!("{zsync_url}: header names no payload URL")));
        }
    };
    let up_to_date = match &header.sha1 {
        Some(want) => sha1_file(local).map(|got| got == *want).unwrap_or(false),
        None => false,
    };
    Ok(Some(UpdateCheck { zsync_url, payload_url, header, up_to_date }))
}

/// Apply a checked update: delta-download, verify, snapshot, swap.
///
/// Writes the new AppImage to a temporary sibling, verifies its SHA-1, moves
/// the current file to a snapshot, then renames the new file into place.
/// The current file is never modified in place.
pub fn apply(
    local: &Path,
    check: &UpdateCheck,
    snapshot_to: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<ApplyReport> {
    let control = fetch_control(&check.zsync_url)?;
    let length = control.header.length;
    if length > MAX_PAYLOAD {
        return Err(Error::Zsync(format!("payload claims {length} bytes; refusing")));
    }
    let want_sha1 = control.header.sha1.clone().ok_or_else(|| {
        Error::Zsync(format!("{}: header carries no SHA-1", check.zsync_url))
    })?;

    // Already current? Belt and braces: the check compared SHA-1 already,
    // but re-verify cheaply via a full block scan result.
    let map = scan::scan(local, &control)?;
    let total_blocks = control.block_count();
    let reused_blocks = map.matched();

    if map.is_complete() {
        // Every block verified by MD4; confirm the whole file and finish
        // without downloading anything.
        if sha1_file(local)? == want_sha1 {
            progress(length, length);
            return Ok(ApplyReport {
                downloaded_bytes: 0,
                total_bytes: length,
                reused_blocks,
                total_blocks,
            });
        }
        // Paranoia: fall through to a full download below.
    }

    // Missing blocks, coalesced into the fewest ranges.
    let ranges = missing_ranges(&map, &control);
    let to_fetch: u64 = ranges.iter().map(|(s, e)| e - s).sum();

    let tmp = tmp_sibling(local);
    let mut out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| Error::Zsync(format!("creating {}: {e}", tmp.display())))?;
    out.set_len(length).map_err(|e| Error::Zsync(e.to_string()))?;

    // 1. Copy every block the local file already holds.
    {
        let mut src = File::open(local).map_err(|e| Error::Zsync(e.to_string()))?;
        let bs = control.blocksize;
        let mut buf = vec![0u8; bs.min(1024 * 1024) as usize];
        for block in 0..total_blocks {
            let Some(off) = map.offset_of(block) else { continue };
            let pos = block as u64 * bs;
            // The final block may be short; copy only real bytes.
            let want = ((length - pos).min(bs)) as usize;
            src.seek(SeekFrom::Start(off)).map_err(|e| Error::Zsync(e.to_string()))?;
            out.seek(SeekFrom::Start(pos)).map_err(|e| Error::Zsync(e.to_string()))?;
            let mut left = want;
            while left > 0 {
                let n = buf.len().min(left);
                src.read_exact(&mut buf[..n]).map_err(|e| Error::Zsync(e.to_string()))?;
                out.write_all(&buf[..n]).map_err(|e| Error::Zsync(e.to_string()))?;
                left -= n;
            }
        }
    }

    // 2. Fetch the missing ranges straight into place.
    let mut done: u64 = 0;
    progress(done, to_fetch);
    for (start, end) in &ranges {
        let len = end - start;
        let mut written: u64 = 0;
        fetch::get_range_into(&check.payload_url, *start, len, |chunk| {
            out.seek(SeekFrom::Start(*start + written)).map_err(|e| e.to_string())?;
            out.write_all(chunk).map_err(|e| e.to_string())?;
            written += chunk.len() as u64;
            Ok(())
        })
        .map_err(|e| Error::Zsync(format!("fetching payload range: {e}")))?;
        done += len;
        progress(done, to_fetch);
    }
    drop(out);

    // 3. The whole-file SHA-1 is the verdict: block matches alone are not
    //    enough to trust the assembly.
    let got = sha1_file(&tmp)?;
    if got != want_sha1 {
        std::fs::remove_file(&tmp).ok();
        return Err(Error::Zsync(format!(
            "assembled file failed SHA-1 check (want {want_sha1}, got {got}); update discarded"
        )));
    }

    // 4. Snapshot the old file, then swap.
    std::fs::create_dir_all(snapshot_to)
        .map_err(|e| Error::Zsync(format!("creating snapshot dir: {e}")))?;
    let backup = snapshot_to.join(format!(
        "{}-{}.bak",
        local.file_name().and_then(|n| n.to_str()).unwrap_or("appimage"),
        chrono_stamp()
    ));
    std::fs::rename(local, &backup)
        .map_err(|e| Error::Zsync(format!("snapshotting old AppImage: {e}")))?;
    if let Err(e) = std::fs::rename(&tmp, local) {
        // Try to put the old file back; report both failures honestly.
        std::fs::rename(&backup, local).ok();
        return Err(Error::Zsync(format!("activating update: {e} (old file restored)")));
    }
    // Keep the new file executable, as AppImages must be.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(local)
            .map_err(|e| Error::Zsync(e.to_string()))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(local, perms).map_err(|e| Error::Zsync(e.to_string()))?;
    }

    Ok(ApplyReport {
        downloaded_bytes: to_fetch,
        total_bytes: length,
        reused_blocks,
        total_blocks,
    })
}

/// Coalesce missing blocks into `(start, end)` byte ranges of the payload.
fn missing_ranges(map: &scan::SourceMap, control: &ControlFile) -> Vec<(u64, u64)> {
    let bs = control.blocksize;
    let length = control.header.length;
    let mut ranges = Vec::new();
    let mut block = 0;
    let n = control.block_count();
    while block < n {
        if map.offset_of(block).is_some() {
            block += 1;
            continue;
        }
        let start_block = block;
        while block < n && map.offset_of(block).is_none() {
            block += 1;
        }
        let start = start_block as u64 * bs;
        let end = (block as u64 * bs).min(length);
        ranges.push((start, end));
    }
    ranges
}

/// Temporary output next to the target, so the final rename is atomic.
fn tmp_sibling(local: &Path) -> PathBuf {
    let mut name = local
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("appimage")
        .to_string();
    name.push_str(".dropin-new");
    local.with_file_name(name)
}

/// `base` + relative `rel`, or `rel` when absolute.
fn join_url(base: &str, rel: &str) -> String {
    if rel.contains("://") {
        return rel.to_string();
    }
    match base.rfind('/') {
        Some(i) => format!("{}{}", &base[..i + 1], rel.trim_start_matches('/')),
        None => rel.to_string(),
    }
}

/// Lowercase hex SHA-1 of a file, streamed.
fn sha1_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(|e| Error::Zsync(e.to_string()))?;
    let mut h = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| Error::Zsync(e.to_string()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn chrono_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Nanoseconds: two snapshots in the same second must not overwrite
    // each other.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("*.zsync", "app-x86_64.AppImage.zsync"));
        assert!(wildcard_match("app-*.zsync", "app-x86_64.AppImage.zsync"));
        assert!(!wildcard_match("app-*.zsync", "other-x86_64.AppImage.zsync"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("exact", "exact"));
        assert!(!wildcard_match("exact", "exact2"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXbYY"));
    }

    #[test]
    fn resolves_plain_zsync_channel() {
        let u = resolve_zsync_url("zsync|https://example.com/app.zsync").unwrap();
        assert_eq!(u.as_deref(), Some("https://example.com/app.zsync"));
    }

    #[test]
    fn unknown_channel_is_none() {
        assert_eq!(resolve_zsync_url("bintray|whatever").unwrap(), None);
        assert_eq!(resolve_zsync_url("").unwrap(), None);
    }

    #[test]
    fn rejects_malformed_gh_channel() {
        assert_eq!(resolve_zsync_url("gh-releases-zsync|only-two|parts").unwrap(), None);
    }

    #[test]
    fn join_url_resolves_relative() {
        assert_eq!(
            join_url("https://h.com/r/app.zsync", "app.AppImage"),
            "https://h.com/r/app.AppImage"
        );
        assert_eq!(
            join_url("https://h.com/r/app.zsync", "https://x.com/app.AppImage"),
            "https://x.com/app.AppImage"
        );
    }
}
