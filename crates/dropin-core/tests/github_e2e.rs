//! GitHub release tracking: ETag caching, asset download, version flow.
//!
//! A mock GitHub API serves `/repos/o/r/releases/latest` with ETag support
//! and a fake AppImage asset. Proves the client caches (304 on second hit),
//! picks the right asset, and the version logic gates updates.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use dropin_core::github;

const ETAG: &str = "\"abc123\"";

fn release_json(asset_url: &str) -> String {
    format!(
        r#"{{"tag_name": "v2.0.0", "name": "2.0", "assets": [
            {{"name": "app-2.0.0-x86_64.AppImage", "browser_download_url": "{asset_url}", "size": 14}},
            {{"name": "app-2.0.0-x86_64.AppImage.zsync", "browser_download_url": "{asset_url}.zsync", "size": 3}},
            {{"name": "app-2.0.0-aarch64.AppImage", "browser_download_url": "{asset_url}-arm", "size": 14}}
        ]}}"#
    )
}

/// Mock GitHub API: latest release with ETag, plus the asset bytes.
/// Counts API hits to prove caching works.
fn serve(tag: &str, asset_bytes: Vec<u8>) -> (String, Arc<AtomicUsize>, PathBuf) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let asset_bytes = Arc::new(asset_bytes);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let base2 = base.clone();
    thread::spawn(move || {
        for stream in listener.incoming().take(10) {
            let mut stream = stream.unwrap();
            let mut req = [0u8; 4096];
            let n = stream.read(&mut req).unwrap_or(0);
            let req = String::from_utf8_lossy(&req[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let inm = req
                .lines()
                .find_map(|l| {
                    let ll = l.to_ascii_lowercase();
                    ll.strip_prefix("if-none-match:").map(|v| v.trim().to_string())
                })
                .unwrap_or_default();
            if path == "/repos/o/r/releases/latest" {
                hits2.fetch_add(1, Ordering::SeqCst);
                if inm == ETAG {
                    let _ = stream.write_all(b"HTTP/1.1 304 Not Modified\r\n\r\n");
                    continue;
                }
                let body = release_json(&format!("{base2}/dl/app.AppImage"));
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {ETAG}\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            } else if path == "/dl/app.AppImage" {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    asset_bytes.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&asset_bytes);
            } else {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            }
        }
    });
    let dir = std::env::temp_dir().join(format!("dropin-gh-test-{tag}"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    (base, hits, dir)
}

#[test]
fn etag_cache_avoids_second_hit() {
    let (base, hits, dir) = serve("etag", b"fake-appimage".to_vec());

    // First call: 200, caches ETag + body.
    let (r1, cached1) = github::latest_release_from(&base, "o/r", &dir).unwrap();
    assert!(!cached1);
    assert_eq!(r1.tag, "v2.0.0");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // Second call: sends If-None-Match, gets 304, uses cache.
    let (r2, cached2) = github::latest_release_from(&base, "o/r", &dir).unwrap();
    assert!(cached2);
    assert_eq!(r2.tag, "v2.0.0");
    assert_eq!(hits.load(Ordering::SeqCst), 2); // hit, but 304 = no quota cost

    // The cache file exists.
    assert!(dir.join("github_cache").join("o__r.json").exists());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn asset_download_and_version_gate() {
    let (base, _hits, dir) = serve("asset", b"fake-appimage-bytes".to_vec());

    let (release, _) = github::latest_release_from(&base, "o/r", &dir).unwrap();
    let asset = github::pick_appimage_asset(&release).unwrap();
    assert_eq!(asset.name, "app-2.0.0-x86_64.AppImage");

    // Download the asset.
    let dest = dir.join("app.AppImage");
    dropin_core::zsync::fetch::download_to_file(&asset.download_url, &dest, 1024 * 1024).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"fake-appimage-bytes");

    // Version gating.
    assert!(github::is_newer(Some("1.9.0"), &release.tag));
    assert!(!github::is_newer(Some("v2.0.0"), &release.tag));
    assert!(!github::is_newer(Some("3.0.0"), &release.tag));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rejects_bad_repo_format() {
    let dir = std::env::temp_dir().join("dropin-gh-test-bad");
    std::fs::create_dir_all(&dir).ok();
    assert!(github::latest_release_from("http://127.0.0.1:1", "not-a-repo", &dir).is_err());
    assert!(github::latest_release_from("http://127.0.0.1:1", "a/b/c", &dir).is_err());
    std::fs::remove_dir_all(&dir).ok();
}
