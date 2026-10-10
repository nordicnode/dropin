//! End-to-end: a real delta update over HTTP with byte ranges.
//!
//! Builds v1 and v2 of a fake AppImage (v2 reuses most of v1's blocks),
//! serves v2 and its .zsync from a local range-capable HTTP server, and
//! applies the update. Proves the delta downloads only the changed blocks
//! and the assembled file verifies by SHA-1.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use dropin_core::zsync::control::HashLengths;
use dropin_core::zsync::rsum::Rsum;
use md4::{Digest as _, Md4};
use sha1::Sha1;

const BLOCKSIZE: usize = 2048;
const BLOCKS: usize = 8;

/// Deterministic block content.
fn block_data(seed: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| ((seed * 31 + i as u64 * 17 + (i >> 4) as u64) % 251) as u8).collect()
}

fn md4_of(data: &[u8]) -> [u8; 16] {
    let mut h = Md4::new();
    h.update(data);
    h.finalize().into()
}

fn sha1_hex(data: &[u8]) -> String {
    format!("{:x}", Sha1::digest(data))
}

/// Minimal .zsync writer for tests (mirrors the format `control` parses).
fn write_zsync(target: &[u8], sha1: &str) -> Vec<u8> {
    let lengths = HashLengths { seq_matches: 2, rsum_bytes: 2, checksum_bytes: 4 };
    let nblocks = target.len().div_ceil(BLOCKSIZE);
    let mut out = format!(
        "zsync: 0.6.2\nFilename: app.AppImage\nBlocksize: {BLOCKSIZE}\nLength: {}\n\
         Hash-Lengths: 2,2,4\nURL: app.AppImage\nSHA-1: {sha1}\n\n",
        target.len()
    )
    .into_bytes();
    for b in 0..nblocks {
        let start = b * BLOCKSIZE;
        let end = (start + BLOCKSIZE).min(target.len());
        let mut window = vec![0u8; BLOCKSIZE];
        window[..end - start].copy_from_slice(&target[start..end]);
        let v = lengths.truncate(Rsum::of(&window));
        out.extend_from_slice(&v.to_be_bytes()[2..4]); // last 2 bytes
        out.extend_from_slice(&md4_of(&window)[..4]);
    }
    out
}

/// Serve `zsync_bytes` at /app.zsync and `payload` at /app.AppImage,
/// honoring Range requests. Returns the base URL. Tracks bytes served
/// for the payload to prove the delta was partial.
fn serve(zsync_bytes: Vec<u8>, payload: Vec<u8>) -> (String, Arc<std::sync::atomic::AtomicU64>) {
    let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let served2 = served.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let zsync_bytes = Arc::new(zsync_bytes);
    let payload = Arc::new(payload);
    thread::spawn(move || {
        for stream in listener.incoming().take(20) {
            let mut stream = stream.unwrap();
            let mut req = [0u8; 4096];
            let n = stream.read(&mut req).unwrap_or(0);
            let req = String::from_utf8_lossy(&req[..n]);
            let (path, range) = parse_req(&req);
            let (body, is_zsync): (&[u8], bool) = match path {
                "/app.zsync" => (&zsync_bytes, true),
                "/app.AppImage" => (&payload, false),
                _ => {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    continue;
                }
            };
            if !is_zsync {
                served2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let (start, end) = match range {
                Some((s, e)) => (s, e.min(body.len() as u64 - 1)),
                None => (0, body.len() as u64 - 1),
            };
            // Count actual payload bytes served.
            if !is_zsync {
                served2.fetch_add(end - start, std::sync::atomic::Ordering::SeqCst);
            }
            let chunk = &body[start as usize..=end as usize];
            let status = if range.is_some() { "206 Partial Content" } else { "200 OK" };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\
                 Content-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\n\r\n",
                chunk.len(),
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(chunk);
        }
    });
    (format!("http://{addr}"), served)
}

fn parse_req(req: &str) -> (&str, Option<(u64, u64)>) {
    let mut lines = req.lines();
    let path = lines.next().unwrap_or("").split_whitespace().nth(1).unwrap_or("/");
    // Leak the path: the test is short-lived; simpler than lifetimes.
    let path: &'static str = Box::leak(path.to_string().into_boxed_str());
    let mut range = None;
    for line in lines {
        // Header names are case-insensitive; ureq sends them lowercase.
        if let Some(v) = line.strip_prefix("range: bytes=").or_else(|| line.strip_prefix("Range: bytes=")) {
            let v = v.trim();
            if let Some((s, e)) = v.split_once('-') {
                if let (Ok(s), Ok(e)) = (s.parse(), e.parse()) {
                    range = Some((s, e));
                }
            }
        }
    }
    (path, range)
}

fn tmpdir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("dropin-zsync-e2e-{name}"));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn delta_update_reuses_blocks_and_verifies() {
    // v1: 8 blocks. v2: same, except blocks 2 and 5 changed, plus a tail.
    let v1: Vec<u8> = (0..BLOCKS).flat_map(|b| block_data(b as u64, BLOCKSIZE)).collect();
    let mut v2 = v1.clone();
    for b in [2usize, 5] {
        let new = block_data(100 + b as u64, BLOCKSIZE);
        v2[b * BLOCKSIZE..(b + 1) * BLOCKSIZE].copy_from_slice(&new);
    }
    v2.extend_from_slice(&block_data(999, 100)); // new tail bytes

    let sha1 = sha1_hex(&v2);
    let zsync_bytes = write_zsync(&v2, &sha1);
    let (base, served) = serve(zsync_bytes, v2.clone());

    let dir = tmpdir("delta");
    let local = dir.join("app.AppImage");
    std::fs::write(&local, &v1).unwrap();
    let snap_dir = dir.join("snaps");

    let check = dropin_core::zsync::UpdateCheck {
        zsync_url: format!("{base}/app.zsync"),
        payload_url: format!("{base}/app.AppImage"),
        header: dropin_core::zsync::fetch_header(&format!("{base}/app.zsync")).unwrap(),
        up_to_date: false,
    };

    let mut progress = vec![];
    let report = dropin_core::zsync::apply(&local, &check, &snap_dir, &mut |d, t| {
        progress.push((d, t));
    })
    .unwrap();

    // The assembled file is exactly v2.
    assert_eq!(std::fs::read(&local).unwrap(), v2);
    // 6 of 8 blocks reused; only 2 changed blocks (+ tail) fetched.
    assert_eq!(report.reused_blocks, 6, "reused {}", report.reused_blocks);
    assert_eq!(report.total_blocks, 9); // v2 is 8 blocks + partial tail block
    assert!(report.downloaded_bytes < v2.len() as u64 / 2, "fetched {}", report.downloaded_bytes);
    assert!(!progress.is_empty());

    // The old file was snapshotted.
    let snaps: Vec<_> = std::fs::read_dir(&snap_dir).unwrap().collect();
    assert_eq!(snaps.len(), 1);
    let snap_path = snaps[0].as_ref().unwrap().path();
    assert_eq!(std::fs::read(&snap_path).unwrap(), v1);

    // And the server saw far fewer payload bytes than the full file.
    let payload_bytes = served.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        payload_bytes < v2.len() as u64,
        "server served {payload_bytes} of {} payload bytes",
        v2.len()
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn up_to_date_file_downloads_nothing() {
    let v1: Vec<u8> = (0..4).flat_map(|b| block_data(b as u64, BLOCKSIZE)).collect();
    let sha1 = sha1_hex(&v1);
    let zsync_bytes = write_zsync(&v1, &sha1);
    let (base, served) = serve(zsync_bytes, v1.clone());

    let dir = tmpdir("uptodate");
    let local = dir.join("app.AppImage");
    std::fs::write(&local, &v1).unwrap();

    let zsync_url = format!("{base}/app.zsync");
    let check = dropin_core::zsync::check(&local, &format!("zsync|{zsync_url}"))
        .unwrap()
        .unwrap();
    assert!(check.up_to_date);

    std::fs::remove_dir_all(&dir).ok();
    // Only the header was fetched; the payload was never touched.
    assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn tampered_assembly_is_rejected() {
    // The server serves a payload whose bytes don't match the .zsync's
    // SHA-1: apply must refuse to swap it in.
    let v1: Vec<u8> = (0..4).flat_map(|b| block_data(b as u64, BLOCKSIZE)).collect();
    let mut evil = v1.clone();
    evil[100] ^= 0xff;
    let sha1 = sha1_hex(&v1); // .zsync describes v1...
    let zsync_bytes = write_zsync(&v1, &sha1);
    let (base, _served) = serve(zsync_bytes, evil); // ...but the server sends tampered bytes

    let dir = tmpdir("tamper");
    let local = dir.join("app.AppImage");
    // Local file is all zeros: nothing reused, everything fetched.
    std::fs::write(&local, vec![0u8; v1.len()]).unwrap();
    let snap_dir = dir.join("snaps");

    let zsync_url = format!("{base}/app.zsync");
    let header = dropin_core::zsync::fetch_header(&zsync_url).unwrap();
    let check = dropin_core::zsync::UpdateCheck {
        zsync_url: zsync_url.clone(),
        payload_url: format!("{base}/app.AppImage"),
        header,
        up_to_date: false,
    };
    let res = dropin_core::zsync::apply(&local, &check, &snap_dir, &mut |_, _| {});
    assert!(res.is_err(), "tampered payload must be rejected");
    // Original file untouched, no snapshot taken.
    assert_eq!(std::fs::read(&local).unwrap(), vec![0u8; v1.len()]);
    assert!(std::fs::read_dir(&snap_dir).is_err() || std::fs::read_dir(&snap_dir).unwrap().count() == 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn gh_channel_pattern_matching() {
    // resolve_zsync_url is unit-tested; here we just confirm the prefix
    // dispatches to the GitHub path (network not exercised).
    let r = dropin_core::zsync::resolve_zsync_url("gh-releases-zsync|a|b");
    assert!(r.unwrap().is_none());
}
