//! Per-format install handlers. Each handler implements:
//!   - `plan(src)` — dry run: what *would* happen (for `dropin info`)
//!   - `install(src, opts)` — do it, returning every managed path for the ledger

pub mod appimage;
pub mod deb;
pub mod flatpak;
pub mod proton;
pub mod rpm;
pub mod script;
pub mod tarball;
pub mod wine;
pub mod wine_setup;

use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Removes a freshly-created install root if the handler fails partway
/// through. Disarm it once the install is complete and recorded.
pub struct RootGuard {
    path: PathBuf,
    armed: bool,
}

impl RootGuard {
    pub fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

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
    /// Runner for Windows apps: None = auto-detect (game -> Proton, app -> Wine).
    pub runner: Option<proton::Runner>,
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

/// Reject a second install of the same slug before running anything that
/// could leave files behind. Handlers that create a unique root already
/// check that root; this covers prefix-based (Wine) and script installs.
pub fn ensure_not_installed(slug: &str) -> Result<()> {
    if crate::ledger::Ledger::open()
        .and_then(|l| l.get(slug))
        .is_ok()
    {
        return Err(Error::Refused(format!(
            "'{slug}' is already installed — remove it first with `dropin remove {slug}`"
        )));
    }
    Ok(())
}

/// The privilege-escalation command to use: pkexec when available (graphical
/// prompt), otherwise sudo.
pub fn escalate() -> &'static str {
    if crate::have("pkexec") {
        "pkexec"
    } else {
        "sudo"
    }
}

/// Remove a package that was installed with `--system`, using the distro's
/// package manager. This is the counterpart to `deb::install_system` and
/// `rpm::install_system`; without it, `dropin remove` would only forget the
/// ledger entry and leave the package installed.
pub fn remove_system_package(name: &str) -> Result<String> {
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let (pm, args): (&str, Vec<&str>) = if os_release.contains("ID_LIKE=debian")
        || os_release.contains("\nID=debian")
        || os_release.contains("\nID=ubuntu")
    {
        ("apt", vec!["remove", "-y", name])
    } else if os_release.contains("fedora")
        || os_release.contains("rhel")
        || os_release.contains("ID_LIKE=\"rhel")
    {
        ("dnf", vec!["remove", "-y", name])
    } else if os_release.contains("arch") {
        ("pacman", vec!["-R", "--noconfirm", name])
    } else if os_release.contains("suse") || os_release.contains("opensuse") {
        ("zypper", vec!["--non-interactive", "remove", name])
    } else {
        return Err(Error::Refused(format!(
            "no supported package manager to remove system package '{name}'"
        )));
    };
    let escalate = escalate();
    let status = std::process::Command::new(escalate)
        .arg(pm)
        .args(&args)
        .status()
        .map_err(|e| Error::Command(format!("system removal failed: {e}")))?;
    if !status.success() {
        return Err(Error::Command(format!(
            "system package removal failed for '{name}'"
        )));
    }
    Ok(format!("Removed system package '{name}' via {pm}."))
}
