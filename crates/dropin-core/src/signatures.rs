//! Detached-signature checking: real verification when possible, key
//! continuity otherwise.
//!
//! When a `.asc`/`.sig` detached signature accompanies a download, dropin:
//!
//! 1. Parses it to extract the issuer key fingerprint.
//! 2. Tries `gpg --verify` against the user's keyring. If the signer's
//!    public key is present, this is a real cryptographic verification.
//! 3. If gpg is unavailable or the key isn't in the keyring, falls back to
//!    continuity pinning (TOFU): the issuer fingerprint is pinned on first
//!    install, and updates must be signed by the same key.
//!
//! Continuity is not verification. It catches a key swap between releases,
//! but it cannot prove a signature is valid without the public key. The CLI
//! says which mode was used.

use crate::{Error, Result};
use std::path::Path;

/// Result of checking a file for a detached signature.
pub enum SigCheck {
    /// No signature file found; install proceeds as unverified.
    None,
    /// Signature found; issuer key ID (hex). `verified` is true when gpg
    /// confirmed the signature against a public key in the user's keyring.
    Found { fingerprint: String, verified: bool },
    /// A signature file exists but could not be parsed. Treat this as
    /// suspicious rather than as "no signature".
    Invalid(String),
    /// The signature is present and gpg says it is BAD (the file was
    /// modified after signing). This is a hard failure.
    Bad(String),
}

/// Look for `<file>.asc` or `<file>.sig` next to `file`.
pub fn find_signature(file: &Path) -> Option<std::path::PathBuf> {
    for ext in ["asc", "sig"] {
        let mut s = file.as_os_str().to_owned();
        s.push(format!(".{ext}"));
        let p = std::path::PathBuf::from(s);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Parse a detached OpenPGP signature and extract the issuer key ID.
/// Handles both armored and binary signatures.
pub fn issuer_fingerprint(sig_path: &Path) -> Result<String> {
    use sequoia_openpgp::parse::Parse;

    let data = std::fs::read(sig_path)?;
    // De-armor if needed.
    let raw: Vec<u8> = if data.starts_with(b"-----BEGIN") {
        dearmor(&data)?
    } else {
        data
    };

    let mut ppr = sequoia_openpgp::parse::PacketParser::from_bytes(&raw)
        .map_err(|e| Error::Unsupported(format!("could not parse signature: {e}")))?;
    while let sequoia_openpgp::parse::PacketParserResult::Some(pp) = ppr {
        let (packet, next) = pp
            .next()
            .map_err(|e| Error::Unsupported(format!("packet error: {e}")))?;
        ppr = next;
        if let sequoia_openpgp::packet::Packet::Signature(sig) = packet {
            // Prefer full fingerprints; fall back to key IDs.
            if let Some(fp) = sig.issuer_fingerprints().next() {
                return Ok(format!("{fp:X}"));
            }
            if let Some(kid) = sig.issuers().next() {
                return Ok(format!("{kid:X}"));
            }
        }
    }
    Err(Error::Unsupported("signature has no issuer".into()))
}

fn dearmor(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    let cursor = std::io::Cursor::new(data);
    let mut rdr = sequoia_openpgp::armor::Reader::from_reader(cursor, None);
    rdr.read_to_end(&mut out)
        .map_err(|e| Error::Unsupported(format!("de-armor failed: {e}")))?;
    Ok(out)
}

/// What gpg said about a signature.
enum GpgResult {
    /// Good signature from a key in the keyring.
    Good,
    /// BAD signature: the file does not match the signature.
    Bad,
    /// The signer's public key isn't in the keyring (or gpg couldn't check).
    NoKey,
    /// gpg isn't installed.
    Unavailable,
}

/// Verify `sig` over `file` with gpg, using the user's keyring.
///
/// `--batch --no-tty` keeps it non-interactive; `--no-auto-key-retrieve`
/// prevents a surprise network lookup. We parse the machine-readable status
/// lines so a missing key is not confused with a bad signature.
fn verify_with_gpg(sig: &Path, file: &Path) -> GpgResult {
    let gpg = if crate::have("gpg") {
        "gpg"
    } else if crate::have("gpg2") {
        "gpg2"
    } else {
        return GpgResult::Unavailable;
    };
    let out = std::process::Command::new(gpg)
        .args([
            "--batch",
            "--no-tty",
            "--no-auto-key-retrieve",
            "--status-fd",
            "1",
            "--verify",
        ])
        .arg(sig)
        .arg(file)
        .output();
    let Ok(out) = out else {
        return GpgResult::Unavailable;
    };
    let status = String::from_utf8_lossy(&out.stdout);
    if status.contains("[GNUPG:] GOODSIG") {
        GpgResult::Good
    } else if status.contains("[GNUPG:] BADSIG") {
        GpgResult::Bad
    } else if status.contains("[GNUPG:] NO_PUBKEY") || status.contains("[GNUPG:] ERRSIG") {
        GpgResult::NoKey
    } else if out.status.success() {
        // gpg succeeded but emitted no status line we recognize.
        GpgResult::Good
    } else {
        GpgResult::NoKey
    }
}

/// Check a file for a signature. Tries real gpg verification first, then
/// falls back to issuer extraction for continuity pinning.
pub fn check(file: &Path) -> SigCheck {
    let Some(sig) = find_signature(file) else {
        return SigCheck::None;
    };
    let fingerprint = match issuer_fingerprint(&sig) {
        Ok(f) => f,
        Err(e) => return SigCheck::Invalid(format!("{}: {e}", sig.display())),
    };
    match verify_with_gpg(&sig, file) {
        GpgResult::Good => SigCheck::Found {
            fingerprint,
            verified: true,
        },
        GpgResult::Bad => SigCheck::Bad(format!(
            "{}: gpg reports a BAD signature (the file was modified after signing)",
            sig.display()
        )),
        GpgResult::NoKey | GpgResult::Unavailable => SigCheck::Found {
            fingerprint,
            verified: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_signature_is_none() {
        let dir = crate::unique_temp_dir("dropin-sig-none");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app.AppImage");
        std::fs::write(&file, b"data").unwrap();
        assert!(matches!(check(&file), SigCheck::None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_signature_is_invalid_not_none() {
        let dir = crate::unique_temp_dir("dropin-sig-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app.AppImage");
        std::fs::write(&file, b"data").unwrap();
        std::fs::write(dir.join("app.AppImage.asc"), b"not a signature").unwrap();
        assert!(matches!(check(&file), SigCheck::Invalid(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpg_verifies_a_real_signature() {
        // Skip when gpg isn't available (CI images usually have it).
        if !crate::have("gpg") {
            return;
        }
        let dir = crate::unique_temp_dir("dropin-sig-gpg");
        std::fs::create_dir_all(&dir).unwrap();
        let gnupg = dir.join("gnupg");
        std::fs::create_dir_all(&gnupg).unwrap();
        // check() runs gpg with the inherited environment, so point it at
        // the throwaway keyring for the duration of this test.
        let old_gnupg = std::env::var_os("GNUPGHOME");
        std::env::set_var("GNUPGHOME", &gnupg);
        let gpg = |args: &[&str]| {
            std::process::Command::new("gpg")
                .env("GNUPGHOME", &gnupg)
                .args(args)
                .output()
                .unwrap()
        };
        // Generate a throwaway key.
        let gen = gpg(&[
            "--batch",
            "--passphrase",
            "",
            "--quick-generate-key",
            "dropin test <test@example.invalid>",
            "ed25519",
            "sign",
            "0",
        ]);
        if !gen.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return; // environment can't generate keys; skip
        }
        let file = dir.join("app.AppImage");
        std::fs::write(&file, b"hello dropin").unwrap();
        let sig = dir.join("app.AppImage.asc");
        let sign = gpg(&[
            "--batch",
            "--armor",
            "--detach-sign",
            "--output",
            sig.to_str().unwrap(),
            file.to_str().unwrap(),
        ]);
        assert!(sign.status.success(), "signing failed");
        match check(&file) {
            SigCheck::Found { verified, .. } => assert!(verified, "gpg should verify"),
            _ => panic!("expected a found signature"),
        }
        // Tamper with the file: gpg must report BAD.
        std::fs::write(&file, b"tampered").unwrap();
        assert!(matches!(check(&file), SigCheck::Bad(_)));
        match old_gnupg {
            Some(v) => std::env::set_var("GNUPGHOME", v),
            None => std::env::remove_var("GNUPGHOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
