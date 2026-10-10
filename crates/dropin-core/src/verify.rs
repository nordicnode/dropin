//! Integrity verification for sideloaded files.
//!
//! Fully offline: dropin never fetches checksums from the network. It only
//! checks files the user already downloaded next to the installer, the way
//! release pages ship them (e.g. `foo.AppImage` + `foo.AppImage.sha256`, or a
//! `SHA256SUMS` file in the same directory).
//!
//! - Checksum found and matches → [`Verification::Verified`]
//! - Checksum found but differs → [`Verification::Mismatch`] (install is refused)
//! - No checksum file → [`Verification::Unverified`] (install proceeds)

use std::path::Path;

/// Companion checksum files dropin looks for, in priority order.
fn candidates(file: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let parent = file.parent().unwrap_or(Path::new("."));
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    // Per-file companions first: foo.AppImage.sha256, foo.AppImage.sha256sum
    for ext in ["sha256", "sha256sum", "SHA256", "sha256.txt"] {
        out.push(parent.join(format!("{name}.{ext}")));
    }
    // Then directory-level manifests.
    for manifest in [
        "SHA256SUMS",
        "SHA256SUMS.txt",
        "sha256sums.txt",
        "CHECKSUMS",
        "checksums.txt",
    ] {
        out.push(parent.join(manifest));
    }
    out.into_iter().filter(|p| p.is_file()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// A checksum file was found and the hash matches.
    Verified { checksum_file: String },
    /// No checksum file found next to the download.
    Unverified,
    /// A checksum was found but does not match — do not install.
    Mismatch {
        expected: String,
        actual: String,
        checksum_file: String,
    },
}

/// Verify `file` (whose SHA-256 hex digest is `sha256`) against companion
/// checksum files in the same directory.
pub fn verify(file: &Path, sha256: &str) -> Verification {
    let file_name = file.file_name().unwrap_or_default().to_string_lossy();
    for candidate in candidates(file) {
        let text = match std::fs::read_to_string(&candidate) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Some(expected) = find_hash(&text, &file_name) {
            let from = candidate
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if expected.eq_ignore_ascii_case(sha256) {
                return Verification::Verified {
                    checksum_file: from,
                };
            }
            return Verification::Mismatch {
                expected,
                actual: sha256.to_string(),
                checksum_file: from,
            };
        }
    }
    Verification::Unverified
}

/// Look for a SHA-256 hex digest for `file_name` in checksum file text.
/// Handles `SHA256SUMS` style (`<hash> [*]<name>`) and bare-hash files
/// (`<hash>` alone, as shipped in `foo.AppImage.sha256`).
fn find_hash(text: &str, file_name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next().unwrap_or("");
        if !is_sha256_hex(hash) {
            continue;
        }
        match parts.next() {
            // Bare hash with nothing else: only meaningful in a per-file
            // companion (foo.AppImage.sha256 contains just the hash).
            None => return Some(hash.to_lowercase()),
            Some(name) => {
                let name = name.trim_start_matches('*');
                // Match by basename: manifests may carry paths.
                let base = name.rsplit('/').next().unwrap_or(name);
                if base == file_name {
                    return Some(hash.to_lowercase());
                }
            }
        }
    }
    None
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dropin-verify-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    const HASH_OF_HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn verifies_per_file_companion() {
        let d = tmpdir("ok");
        let f = write_file(&d, "tool.AppImage", b"hello");
        write_file(&d, "tool.AppImage.sha256", HASH_OF_HELLO.as_bytes());
        match verify(&f, HASH_OF_HELLO) {
            Verification::Verified { .. } => {}
            other => panic!("expected verified, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn verifies_sha256sums_manifest() {
        let d = tmpdir("manifest");
        let f = write_file(&d, "tool.AppImage", b"hello");
        write_file(
            &d,
            "SHA256SUMS",
            format!("{HASH_OF_HELLO} *tool.AppImage\n").as_bytes(),
        );
        assert!(matches!(
            verify(&f, HASH_OF_HELLO),
            Verification::Verified { .. }
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn mismatch_is_detected() {
        let d = tmpdir("bad");
        let f = write_file(&d, "tool.AppImage", b"hello");
        write_file(&d, "tool.AppImage.sha256", "0".repeat(64).as_bytes());
        match verify(&f, HASH_OF_HELLO) {
            Verification::Mismatch { .. } => {}
            other => panic!("expected mismatch, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_checksum_is_unverified() {
        let d = tmpdir("none");
        let f = write_file(&d, "tool.AppImage", b"hello");
        assert_eq!(verify(&f, HASH_OF_HELLO), Verification::Unverified);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ignores_manifest_entries_for_other_files() {
        let d = tmpdir("other");
        let f = write_file(&d, "tool.AppImage", b"hello");
        write_file(
            &d,
            "SHA256SUMS",
            format!("{HASH_OF_HELLO} *something-else.AppImage\n").as_bytes(),
        );
        assert_eq!(verify(&f, HASH_OF_HELLO), Verification::Unverified);
        let _ = std::fs::remove_dir_all(&d);
    }
}
