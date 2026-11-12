//! Signature continuity with key pinning (TOFU + continuity).
//!
//! When a `.asc`/`.sig` detached signature accompanies a download, dropin
//! parses it to extract the issuer key fingerprint. On first install the
//! fingerprint is pinned (trust on first use, over HTTPS). On updates, a
//! changed, missing, or unreadable signature is refused — mirroring Android's
//! signature-continuity rule. This catches supply-chain key swaps even
//! without a PKI.
//!
//! Scope: this is continuity checking, not cryptographic verification. We do
//! not have the publisher's public key, so we cannot prove the signature is
//! valid; we can only prove the signer identity did not change between
//! releases. The README describes it the same way.

use crate::{Error, Result};
use std::path::Path;

/// Result of checking a file for a detached signature.
pub enum SigCheck {
    /// No signature file found; install proceeds as unverified.
    None,
    /// Signature found; issuer key ID (hex).
    Found { fingerprint: String },
    /// A signature file exists but could not be parsed. Treat this as
    /// suspicious rather than as "no signature".
    Invalid(String),
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
/// Handles both armored and binary signatures. The key ID is enough for
/// continuity pinning (TOFU): a changed key is detected even though we
/// don't do full cryptographic verification without the public key.
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
    // sequoia's armor Reader::from_reader returns a Result in 1.x? No —
    // it returns the Reader directly; errors surface on read.
    let cursor = std::io::Cursor::new(data);
    let mut rdr = sequoia_openpgp::armor::Reader::from_reader(cursor, None);
    rdr.read_to_end(&mut out)
        .map_err(|e| Error::Unsupported(format!("de-armor failed: {e}")))?;
    Ok(out)
}

/// Check a file for a signature; return the issuer fingerprint if found.
pub fn check(file: &Path) -> SigCheck {
    match find_signature(file) {
        Some(sig) => match issuer_fingerprint(&sig) {
            Ok(fingerprint) => SigCheck::Found { fingerprint },
            Err(e) => SigCheck::Invalid(format!("{}: {e}", sig.display())),
        },
        None => SigCheck::None,
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
}
