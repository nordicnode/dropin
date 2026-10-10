//! Automated Wine setup: eliminate all manual tinkering.
//!
//! When Wine is missing, dropin can set it up automatically:
//! 1. Try the system package manager (via polkit/sudo) — proper integration,
//!    all dependencies handled by the distro.
//! 2. Fall back to a portable Wine build downloaded to ~/.local — rootless,
//!    works on any distro.
//!
//! This is explicit user-initiated setup, not background networking.

use crate::{Error, Result};
use std::path::PathBuf;

/// Where portable Wine lives when we download it ourselves.
pub fn portable_wine_dir() -> PathBuf {
    crate::home_dir().join(".local/share/dropin/wine")
}

/// The wine binary to use: system wine first, then our portable copy.
pub fn wine_binary() -> Option<PathBuf> {
    for name in ["wine64", "wine"] {
        if let Ok(path) = which(name) {
            return Some(path);
        }
    }
    let portable = portable_wine_dir().join("bin/wine");
    if portable.exists() {
        return Some(portable);
    }
    None
}

fn which(name: &str) -> Result<PathBuf> {
    let out = crate::run("which", &[name])?;
    let path = out.trim();
    if path.is_empty() {
        return Err(Error::Command(format!("{name} not found")));
    }
    Ok(PathBuf::from(path))
}

/// Detect the distro ID from /etc/os-release.
pub fn distro_id() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("ID="))
                .map(|l| l.trim_start_matches("ID=").trim_matches('"').to_lowercase())
        })
        .unwrap_or_default()
}

/// The package manager install command for Wine on this distro.
/// Returns (command, args) to run with privilege escalation.
pub fn package_install_cmd() -> Option<(String, Vec<String>)> {
    let id = distro_id();
    let id_like = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("ID_LIKE=")).map(|l| {
                l.trim_start_matches("ID_LIKE=")
                    .trim_matches('"')
                    .to_lowercase()
            })
        })
        .unwrap_or_default();

    let is_debian = [
        "debian",
        "ubuntu",
        "linuxmint",
        "pop",
        "zorin",
        "elementary",
    ]
    .iter()
    .any(|d| id == *d || id_like.contains(d));
    let is_fedora = ["fedora", "rhel", "centos", "rocky", "almalinux"]
        .iter()
        .any(|d| id == *d || id_like.contains(d));
    let is_arch = ["arch", "manjaro", "endeavouros", "garuda"]
        .iter()
        .any(|d| id == *d || id_like.contains(d));
    let is_suse = ["opensuse", "opensuse-tumbleweed", "opensuse-leap", "sles"]
        .iter()
        .any(|d| id == *d || id_like.contains(d));

    if is_debian {
        // Enable 32-bit arch for wine32, then install.
        Some((
            "sh".into(),
            vec![
                "-c".into(),
                "dpkg --add-architecture i386 && apt update && apt install -y wine wine32".into(),
            ],
        ))
    } else if is_fedora {
        Some((
            "dnf".into(),
            vec!["install".into(), "-y".into(), "wine".into()],
        ))
    } else if is_arch {
        Some((
            "pacman".into(),
            vec!["-S".into(), "--noconfirm".into(), "wine".into()],
        ))
    } else if is_suse {
        Some((
            "zypper".into(),
            vec!["install".into(), "-y".into(), "wine".into()],
        ))
    } else {
        None
    }
}

/// Install Wine via the system package manager, using polkit (GUI) or sudo (CLI).
/// Returns Ok(true) if Wine is now available.
pub fn install_via_package_manager() -> Result<bool> {
    let (cmd, args) = package_install_cmd().ok_or_else(|| {
        Error::Unsupported("unsupported distro for automatic Wine install".into())
    })?;

    // Prefer pkexec (graphical prompt) when available, fall back to sudo.
    let escalate = if crate::have("pkexec") {
        "pkexec"
    } else {
        "sudo"
    };
    let arg_strs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let status = std::process::Command::new(escalate)
        .arg(&cmd)
        .args(&arg_strs)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| Error::Command(format!("couldn't run {escalate}: {e}")))?;

    if !status.success() {
        return Err(Error::Command(format!(
            "{escalate} {cmd} failed — Wine wasn't installed"
        )));
    }

    Ok(wine_binary().is_some())
}

/// Download a portable Wine build (Kron4ek's builds) to ~/.local.
/// This is rootless and works on any distro with glibc 2.27+.
/// Requires user-approved network access.
pub fn install_portable_wine(progress: &dyn Fn(&str)) -> Result<PathBuf> {
    let dir = portable_wine_dir();
    std::fs::create_dir_all(&dir)?;

    progress("Finding the latest portable Wine build…");

    // Query GitHub API for the latest release.
    let api_out = std::process::Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "30",
            "https://api.github.com/repos/Kron4ek/Wine-Builds/releases/latest",
        ])
        .output()
        .map_err(|e| Error::Command(format!("couldn't reach GitHub: {e}")))?;

    if !api_out.status.success() {
        return Err(Error::Command("couldn't fetch Wine release info".into()));
    }

    let api_text = String::from_utf8_lossy(&api_out.stdout);
    // Find a suitable asset: prefer wine-tkg (has patches), amd64, .tar.xz
    let asset_url = find_wine_asset(&api_text)
        .ok_or_else(|| Error::Command("couldn't find a suitable Wine build".into()))?;

    progress("Downloading Wine (this is a large download, please wait)…");
    let tarball = dir.join("wine.tar.xz");
    let status = std::process::Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "600",
            "-o",
            tarball.to_str().unwrap(),
            &asset_url,
        ])
        .status()
        .map_err(|e| Error::Command(format!("download failed: {e}")))?;

    if !status.success() || !tarball.exists() {
        return Err(Error::Command("Wine download failed".into()));
    }

    progress("Extracting Wine…");
    let status = std::process::Command::new("tar")
        .args([
            "-xf",
            tarball.to_str().unwrap(),
            "-C",
            dir.to_str().unwrap(),
            "--strip-components=1",
        ])
        .status()
        .map_err(|e| Error::Command(format!("extraction failed: {e}")))?;

    let _ = std::fs::remove_file(&tarball);
    if !status.success() {
        return Err(Error::Command("Wine extraction failed".into()));
    }

    let wine_bin = dir.join("bin/wine");
    if !wine_bin.exists() {
        return Err(Error::Command(
            "Wine binary not found after extraction".into(),
        ));
    }

    progress("Wine is ready.");
    Ok(wine_bin)
}

/// Parse GitHub API JSON to find a Wine asset URL.
/// Prefers tkg builds, amd64, .tar.xz format.
fn find_wine_asset(api_json: &str) -> Option<String> {
    // Simple string scanning: find browser_download_url values.
    let mut best: Option<(i32, String)> = None;
    for line in api_json.lines() {
        let line = line.trim();
        if let Some(url) = line
            .strip_prefix("\"browser_download_url\": \"")
            .and_then(|s| s.strip_suffix("\","))
            .or_else(|| {
                line.strip_prefix("\"browser_download_url\":\"")
                    .and_then(|s| s.strip_suffix('"'))
            })
        {
            let url = url.trim_matches('"').trim_matches(',').to_string();
            if !url.ends_with(".tar.xz") {
                continue;
            }
            // Score: prefer amd64, prefer tkg, avoid proton (needs Steam runtime).
            let mut score = 0;
            if url.contains("amd64") || url.contains("x86_64") {
                score += 10;
            }
            if url.contains("tkg") {
                score += 5;
            }
            if url.contains("proton") {
                score -= 20;
            }
            if url.contains("staging") {
                score += 2;
            }
            match &best {
                Some((s, _)) if *s >= score => {}
                _ => best = Some((score, url)),
            }
        }
    }
    best.map(|(_, url)| url)
}

/// Full automated setup: try package manager first, fall back to portable.
/// The `progress` callback receives human-readable status updates.
pub fn auto_setup(progress: &dyn Fn(&str)) -> Result<PathBuf> {
    // Already have it?
    if let Some(bin) = wine_binary() {
        return Ok(bin);
    }

    // Try system package manager first (best integration).
    if package_install_cmd().is_some() {
        progress("Installing Wine with your system package manager…");
        match install_via_package_manager() {
            Ok(true) => {
                if let Some(bin) = wine_binary() {
                    progress("Wine installed successfully.");
                    return Ok(bin);
                }
            }
            Ok(false) | Err(_) => {
                progress("System install didn't work, trying portable Wine…");
            }
        }
    }

    // Fall back to portable.
    install_portable_wine(progress)
}

/// Where DXVK builds are cached.
pub fn dxvk_dir() -> PathBuf {
    crate::home_dir().join(".local/share/dropin/dxvk")
}

/// Set up DXVK (Direct3D-to-Vulkan) in a Wine prefix for better game performance.
/// Downloads the latest DXVK release and symlinks the DLLs into the prefix,
/// like Lutris does. Requires user-approved network access.
pub fn setup_dxvk(prefix: &std::path::Path, progress: &dyn Fn(&str)) -> Result<()> {
    let dir = dxvk_dir();
    std::fs::create_dir_all(&dir)?;

    // Check if we already have DXVK downloaded.
    let existing = std::fs::read_dir(&dir).ok().and_then(|mut entries| {
        entries.find_map(|e| {
            e.ok().and_then(|e| {
                let p = e.path();
                if p.is_dir() && p.join("x64").exists() {
                    Some(p)
                } else {
                    None
                }
            })
        })
    });

    let dxvk_path = match existing {
        Some(p) => p,
        None => {
            progress("Downloading DXVK for better game performance…");
            download_dxvk(&dir, progress)?
        }
    };

    progress("Setting up DXVK in the Wine prefix…");
    let wine_bin = wine_binary().ok_or_else(|| Error::Command("wine not found".into()))?;

    // Symlink DLLs like Lutris does.
    for (src_dir, dest_dir) in [
        (
            dxvk_path.join("x64"),
            prefix.join("drive_c/windows/system32"),
        ),
        (
            dxvk_path.join("x32"),
            prefix.join("drive_c/windows/syswow64"),
        ),
    ] {
        std::fs::create_dir_all(&dest_dir)?;
        for dll in ["d3d9.dll", "d3d10core.dll", "d3d11.dll", "dxgi.dll"] {
            let src = src_dir.join(dll);
            let dest = dest_dir.join(dll);
            if src.exists() {
                let _ = std::fs::remove_file(&dest);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&src, &dest).ok();
            }
        }
    }

    // Set DLL overrides via wine reg.
    let _ = std::process::Command::new(&wine_bin)
        .env("WINEPREFIX", prefix)
        .args([
            "reg",
            "add",
            "HKCU\\Software\\Wine\\DllOverrides",
            "/v",
            "d3d9",
            "/d",
            "native",
            "/f",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    for dll in ["d3d10core", "d3d11", "dxgi"] {
        let _ = std::process::Command::new(&wine_bin)
            .env("WINEPREFIX", prefix)
            .args([
                "reg",
                "add",
                "HKCU\\Software\\Wine\\DllOverrides",
                "/v",
                dll,
                "/d",
                "native",
                "/f",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    progress("DXVK is ready.");
    Ok(())
}

fn download_dxvk(dir: &std::path::Path, _progress: &dyn Fn(&str)) -> Result<PathBuf> {
    let api_out = std::process::Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "30",
            "https://api.github.com/repos/doitsujin/dxvk/releases/latest",
        ])
        .output()
        .map_err(|e| Error::Command(format!("couldn't reach GitHub: {e}")))?;

    if !api_out.status.success() {
        return Err(Error::Command("couldn't fetch DXVK release info".into()));
    }

    let api_text = String::from_utf8_lossy(&api_out.stdout);
    let asset_url = api_text
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            l.strip_prefix("\"browser_download_url\": \"")
                .and_then(|s| s.strip_suffix("\","))
                .or_else(|| {
                    l.strip_prefix("\"browser_download_url\":\"")
                        .and_then(|s| s.strip_suffix('"'))
                })
        })
        .map(|s| s.trim_matches('"').trim_matches(',').to_string())
        .find(|u| u.ends_with(".tar.gz") && u.contains("dxvk"))
        .ok_or_else(|| Error::Command("couldn't find a DXVK build".into()))?;

    let tarball = dir.join("dxvk.tar.gz");
    let status = std::process::Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "300",
            "-o",
            tarball.to_str().unwrap(),
            &asset_url,
        ])
        .status()
        .map_err(|e| Error::Command(format!("download failed: {e}")))?;

    if !status.success() {
        return Err(Error::Command("DXVK download failed".into()));
    }

    // Extract; DXVK tarballs contain a top-level dxvk-<version> dir.
    let status = std::process::Command::new("tar")
        .args([
            "-xzf",
            tarball.to_str().unwrap(),
            "-C",
            dir.to_str().unwrap(),
        ])
        .status()
        .map_err(|e| Error::Command(format!("extraction failed: {e}")))?;
    let _ = std::fs::remove_file(&tarball);

    if !status.success() {
        return Err(Error::Command("DXVK extraction failed".into()));
    }

    // Find the extracted dir.
    std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.join("x64").exists())
        .ok_or_else(|| Error::Command("DXVK not found after extraction".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_detection_works() {
        // Just verify it doesn't panic and returns something sane.
        let id = distro_id();
        assert!(id.len() < 50);
    }

    #[test]
    fn wine_asset_parsing() {
        // Format matches GitHub API: each asset URL on its own line.
        let fake_json = r#"
"browser_download_url": "https://example.com/wine-9.0-amd64.tar.xz",
"browser_download_url": "https://example.com/wine-9.0-tkg-amd64.tar.xz",
"browser_download_url": "https://example.com/proton-9.0.tar.xz",
"#;
        let url = find_wine_asset(fake_json).unwrap();
        assert!(url.contains("tkg"), "should prefer tkg: {url}");
    }
}
