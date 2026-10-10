//! Per-format install handlers. Each handler implements:
//!   - `plan(src)` — dry run: what *would* happen (for `dropin info`)
//!   - `install(src, opts)` — do it, returning every managed path for the ledger

pub mod appimage;
pub mod deb;
pub mod flatpak;
pub mod rpm;
pub mod script;
pub mod tarball;
pub mod wine;
pub mod wine_setup;

use crate::{Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Skip interactive confirmations (scripts still need explicit consent).
    pub yes: bool,
    /// Keep the original downloaded file instead of moving it.
    pub keep_source: bool,
    /// Prefer a real system install (needs root via polkit/sudo) where supported.
    pub system: bool,
    /// Allow installer scripts internet access inside the sandbox.
    pub allow_net: bool,
    /// Suppress progress printing (the GUI presents its own UI).
    pub quiet: bool,
}

/// A dry-run description of what installing `src` would do.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub app_name: String,
    pub version: Option<String>,
    pub steps: Vec<String>,
    pub needs_root: bool,
    pub needs_confirmation: bool,
}

/// The outcome of an install: everything the ledger must record.
#[derive(Debug, Clone, Default)]
pub struct InstallOutcome {
    pub name: String,
    pub version: Option<String>,
    pub install_root: Option<PathBuf>,
    /// Every managed path (files, symlinks, .desktop entries, icons).
    pub paths: Vec<PathBuf>,
    /// True when files were installed outside dropin's tracking.
    pub external: bool,
    /// Where to check for updates (e.g. X-AppImage-UpdateInformation).
    pub update_info: Option<String>,
}

/// sha256 of a file, via the system `sha256sum` (fast, no new deps).
pub fn sha256_of(path: &Path) -> Result<String> {
    let out = crate::run(
        "sha256sum",
        &[path
            .to_str()
            .ok_or_else(|| Error::Unsupported("bad path".into()))?],
    )?;
    Ok(out.split_whitespace().next().unwrap_or("").to_string())
}

/// Reject archives trying to escape their destination.
pub fn assert_safe_member(name: &str) -> Result<()> {
    if name.starts_with('/') || name.contains("..") {
        return Err(Error::Refused(format!("unsafe archive member: {name}")));
    }
    Ok(())
}
