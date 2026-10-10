//! GitHub release tracking for apps without a zsync channel.
//!
//! Many AppImages are published as GitHub release assets with no embedded
//! update information. `dropin track <name> --github owner/repo` associates
//! a repo with an installed app; `dropin update` then checks the latest
//! release, and downloads the new AppImage when the tag is newer than the
//! installed version.
//!
//! API use is polite: release metadata is cached with ETags, so a check
//! that finds nothing new costs no rate-limit quota (a 304, not a 200).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::{Error, Result};
use crate::zsync::USER_AGENT;

/// Where ETag cache entries live: `<data>/github_cache/<owner>__<repo>.json`.
fn cache_file(data_dir: &Path, repo: &str) -> PathBuf {
    let safe = repo.replace('/', "__");
    data_dir.join("github_cache").join(format!("{safe}.json"))
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct CacheEntry {
    etag: String,
    body: String,
}

/// A GitHub release, trimmed to what update checks need.
#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub name: String,
    pub assets: Vec<Asset>,
}

/// A release asset.
#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub download_url: String,
    pub size: u64,
}

/// Base URL for the GitHub API. Overridable via `DROPIN_GITHUB_API` for
/// tests and GitHub Enterprise.
pub fn api_base() -> String {
    std::env::var("DROPIN_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".to_string())
}

/// Fetch the latest release for `owner/repo`, using the ETag cache.
///
/// Returns the release and whether the data came from cache.
pub fn latest_release(repo: &str, data_dir: &Path) -> Result<(Release, bool)> {
    latest_release_from(&api_base(), repo, data_dir)
}

/// Fetch the latest release from a specific API base (for tests).
pub fn latest_release_from(
    api_base: &str,
    repo: &str,
    data_dir: &Path,
) -> Result<(Release, bool)> {
    let (owner, name) = repo
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty())
        .ok_or_else(|| Error::Zsync(format!("{repo:?} is not an owner/repo")))?;
    let _ = (owner, name);

    let url = format!("{api_base}/repos/{repo}/releases/latest");
    let cache_path = cache_file(data_dir, repo);
    let cached: Option<CacheEntry> = std::fs::read_to_string(&cache_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());

    let mut req = ureq::get(&url).header("User-Agent", USER_AGENT);
    req = req.header("Accept", "application/vnd.github+json");
    if let Some(c) = &cached {
        req = req.header("If-None-Match", &c.etag);
    }
    let mut res = req.call().map_err(|e| {
        // ureq surfaces HTTP errors as well; recover the status.
        Error::Zsync(format!("GitHub API request failed: {e}"))
    })?;

    let status = res.status().as_u16();
    let body = match status {
        304 => {
            let c = cached.ok_or_else(|| {
                Error::Zsync("server said not-modified but nothing is cached".to_string())
            })?;
            return parse_release(&c.body).map(|r| (r, true));
        }
        200 => {
            let mut text = String::new();
            use std::io::Read;
            res.body_mut().as_reader().read_to_string(&mut text).map_err(|e| {
                Error::Zsync(format!("reading GitHub response: {e}"))
            })?;
            if let Some(etag) = res.headers().get("etag").and_then(|v| v.to_str().ok()) {
                let entry = CacheEntry { etag: etag.to_string(), body: text.clone() };
                if let Some(parent) = cache_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&cache_path, serde_json::to_string(&entry).unwrap_or_default())
                    .ok();
            }
            text
        }
        404 => {
            return Err(Error::Zsync(format!(
                "no releases found for {repo} (repo missing or no releases published)"
            )));
        }
        403 | 429 => {
            // Rate-limited: stale cache is better than nothing.
            if let Some(c) = cached {
                return parse_release(&c.body).map(|r| (r, true));
            }
            return Err(Error::Zsync(
                "GitHub API rate limit reached and nothing cached; try again later".to_string(),
            ));
        }
        s => {
            return Err(Error::Zsync(format!("GitHub API returned status {s}")));
        }
    };
    parse_release(&body).map(|r| (r, false))
}

fn parse_release(body: &str) -> Result<Release> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| Error::Zsync(format!("bad GitHub response: {e}")))?;
    let tag = v.get("tag_name").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let name = v.get("name").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let mut assets = Vec::new();
    if let Some(arr) = v.get("assets").and_then(|a| a.as_array()) {
        for a in arr {
            assets.push(Asset {
                name: a.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                download_url: a
                    .get("browser_download_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string(),
                size: a.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
            });
        }
    }
    if tag.is_empty() {
        return Err(Error::Zsync("GitHub response has no tag_name".to_string()));
    }
    Ok(Release { tag, name, assets })
}

/// Pick the AppImage asset for this machine.
///
/// Skips signatures, checksums, and zsync files. Prefers an asset naming
/// the local architecture; falls back to the first bare `.AppImage`.
pub fn pick_appimage_asset(release: &Release) -> Option<&Asset> {
    let arch = std::env::consts::ARCH; // "x86_64", "aarch64", ...
    let aliases: &[&str] = match arch {
        "x86_64" => &["x86_64", "amd64"],
        "aarch64" => &["aarch64", "arm64"],
        other => &[other],
    };
    let mut fallback = None;
    for a in &release.assets {
        let lower = a.name.to_lowercase();
        if !lower.ends_with(".appimage") {
            continue;
        }
        // Not the payload: metadata riding alongside it.
        if lower.ends_with(".zsync")
            || lower.ends_with(".sig")
            || lower.ends_with(".sha256")
            || lower.ends_with(".md5")
            || lower.ends_with(".sigstore")
        {
            continue;
        }
        if aliases.iter().any(|al| lower.contains(al)) {
            return Some(a);
        }
        if fallback.is_none() {
            fallback = Some(a);
        }
    }
    fallback
}

/// Compare two version strings, tolerating `v` prefixes, dates, and
/// suffixes like `-beta`. Numeric components compare numerically, so
/// `1.2.10` sorts after `1.2.9`.
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    let norm = |s: &str| {
        s.trim()
            .trim_start_matches(['v', 'V'])
            .split(|c: char| !c.is_alphanumeric())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
    };
    let (pa, pb) = (norm(a), norm(b));
    for (x, y) in pa.iter().zip(pb.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(nx), Ok(ny)) => nx.cmp(&ny),
            _ => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    // Shared prefix equal: a longer tail of numbers means newer (1.2 < 1.2.3),
    // but a tail starting with text means prerelease (2.0-beta < 2.0).
    match pa.len().cmp(&pb.len()) {
        Ordering::Equal => Ordering::Equal,
        Ordering::Greater => {
            if pa[pb.len()].parse::<u64>().is_err() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        Ordering::Less => {
            if pb[pa.len()].parse::<u64>().is_err() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
    }
}

/// True when `latest_tag` is newer than `installed` (or installed is unknown).
pub fn is_newer(installed: Option<&str>, latest_tag: &str) -> bool {
    match installed {
        Some(v) if !v.trim().is_empty() => version_cmp(v, latest_tag) == Ordering::Less,
        _ => true, // Unknown version: sync to latest once, then track the tag.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert_eq!(version_cmp("1.2.3", "1.2.4"), Ordering::Less);
        assert_eq!(version_cmp("v1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(version_cmp("1.2.10", "1.2.9"), Ordering::Greater);
        assert_eq!(version_cmp("1.2", "1.2.3"), Ordering::Less);
        assert_eq!(version_cmp("2024.01.15", "2024.02.01"), Ordering::Less);
        assert_eq!(version_cmp("2.0-beta", "2.0"), Ordering::Less);
        assert_eq!(version_cmp("1.0", "1.0"), Ordering::Equal);
    }

    #[test]
    fn newer_detection() {
        assert!(is_newer(Some("1.0"), "v1.1"));
        assert!(!is_newer(Some("1.1"), "v1.1"));
        assert!(!is_newer(Some("2.0"), "v1.1"));
        assert!(is_newer(None, "v1.1"));
        assert!(is_newer(Some(""), "v1.1"));
    }

    #[test]
    fn picks_arch_appimage() {
        let r = Release {
            tag: "v1".into(),
            name: "".into(),
            assets: vec![
                Asset { name: "app-aarch64.AppImage".into(), download_url: "u1".into(), size: 1 },
                Asset { name: "app-x86_64.AppImage".into(), download_url: "u2".into(), size: 2 },
                Asset { name: "app-x86_64.AppImage.zsync".into(), download_url: "u3".into(), size: 3 },
                Asset { name: "app-x86_64.AppImage.sha256".into(), download_url: "u4".into(), size: 4 },
            ],
        };
        let a = pick_appimage_asset(&r).unwrap();
        if std::env::consts::ARCH == "x86_64" {
            assert_eq!(a.name, "app-x86_64.AppImage");
        } else {
            // Non-x86_64 test machines take the arch match or the fallback.
            assert!(a.name.ends_with(".AppImage"));
        }
    }

    #[test]
    fn skips_non_payload_assets() {
        let r = Release {
            tag: "v1".into(),
            name: "".into(),
            assets: vec![
                Asset { name: "app.AppImage.zsync".into(), download_url: "u".into(), size: 1 },
            ],
        };
        assert!(pick_appimage_asset(&r).is_none());
    }
}
