//! `.desktop` file handling: generate fresh entries or adopt bundled ones
//! (rewriting Exec/Icon paths to their new locations).

use crate::{Error, Result, UserDirs};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: String,
    pub icon: Option<String>,
    pub comment: Option<String>,
    pub categories: Option<String>,
    pub terminal: bool,
}

impl DesktopEntry {
    /// Path of the menu entry dropin will write for `slug`.
    pub fn target_path(slug: &str) -> PathBuf {
        UserDirs::new()
            .desktop_entries
            .join(format!("dropin-{slug}.desktop"))
    }

    pub fn write(&self, slug: &str) -> Result<PathBuf> {
        let dirs = UserDirs::new();
        dirs.ensure()?;
        let path = Self::target_path(slug);
        let mut out = String::from("[Desktop Entry]\n");
        out.push_str("Type=Application\n");
        out.push_str(&format!("Name={}\n", self.name));
        out.push_str(&format!("Exec={}\n", self.exec));
        if let Some(icon) = &self.icon {
            out.push_str(&format!("Icon={icon}\n"));
        }
        if let Some(comment) = &self.comment {
            out.push_str(&format!("Comment={comment}\n"));
        }
        out.push_str(&format!(
            "Categories={}\n",
            self.categories.as_deref().unwrap_or("Utility;")
        ));
        out.push_str(&format!(
            "Terminal={}\n",
            if self.terminal { "true" } else { "false" }
        ));
        out.push_str("X-Dropin-Managed=true\n");
        std::fs::write(&path, out)?;
        // Best-effort: refresh the menu cache.
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&dirs.desktop_entries)
            .output();
        Ok(path)
    }
}

/// Parse the `[Desktop Entry]` group of a .desktop file into a flat map.
pub fn parse_desktop_file(path: &Path) -> Result<HashMap<String, String>> {
    let text = std::fs::read_to_string(path)?;
    let mut map = HashMap::new();
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            // Prefer the un-localized key (Name over Name[en]).
            if !k.contains('[') {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    if map.is_empty() {
        return Err(Error::Unsupported(format!(
            "no [Desktop Entry] in {}",
            path.display()
        )));
    }
    Ok(map)
}

/// Adopt a bundled .desktop file: rewrite Exec/Icon to the new install root,
/// install a copy of the icon, and write the managed entry.
/// Returns (menu entry path, extra recorded paths).
pub fn adopt_desktop_file(
    src: &Path,
    slug: &str,
    app_name: &str,
    install_root: &Path,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    adopt_desktop_file_inner(src, slug, app_name, install_root, false)
}

/// Adopt a .desktop file from inside an AppImage. `appimage` is the installed
/// AppImage file itself: the embedded `Exec=AppRun` (the AppImage convention
/// for "run me") is rewritten to it, instead of being joined onto it as if
/// it were a directory.
pub fn adopt_desktop_file_appimage(
    src: &Path,
    slug: &str,
    app_name: &str,
    appimage: &Path,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    adopt_desktop_file_inner(src, slug, app_name, appimage, true)
}

fn adopt_desktop_file_inner(
    src: &Path,
    slug: &str,
    app_name: &str,
    install_root: &Path,
    is_appimage: bool,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    let map = parse_desktop_file(src)?;
    let mut recorded = Vec::new();

    let exec = map
        .get("Exec")
        .map(|e| rewrite_exec(e, install_root, is_appimage))
        .unwrap_or_else(|| install_root.display().to_string());
    let icon = map
        .get("Icon")
        .and_then(|i| install_icon(i, slug, install_root))
        .map(|p| {
            recorded.push(p.clone());
            p.display().to_string()
        });

    let entry = DesktopEntry {
        name: map
            .get("Name")
            .cloned()
            .unwrap_or_else(|| app_name.to_string()),
        exec,
        icon,
        comment: map.get("Comment").cloned(),
        categories: map.get("Categories").cloned(),
        terminal: map.get("Terminal").map(|t| t == "true").unwrap_or(false),
    };
    let path = entry.write(slug)?;
    recorded.push(path.clone());
    Ok((path, recorded))
}

/// Rewrite an Exec line whose binary lived under /usr (or relative) to the
/// new root. Keeps any arguments after the binary. Field codes
/// (%f %F %u %U %i %c %k) pass through untouched.
///
/// For AppImages (`is_appimage`), the embedded Exec is `AppRun` by
/// convention — it means "run the image itself", so it becomes the AppImage
/// path. (Joining it onto the path would produce `<image>/AppRun`, which
/// does not exist.)
fn rewrite_exec(exec: &str, install_root: &Path, is_appimage: bool) -> String {
    let mut parts = exec.split_whitespace();
    let bin = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let new_bin = if is_appimage {
        if bin.starts_with('/') {
            bin.to_string() // absolute inside an AppImage: leave it
        } else {
            install_root.display().to_string()
        }
    } else if bin.starts_with("/usr/") {
        install_root
            .join(bin.trim_start_matches('/'))
            .display()
            .to_string()
    } else if bin.starts_with('/') {
        bin.to_string() // already absolute elsewhere; leave it
    } else {
        install_root.join(bin).display().to_string()
    };
    if rest.is_empty() {
        new_bin
    } else {
        format!("{new_bin} {}", rest.join(" "))
    }
}

/// Install an icon referenced by a .desktop file into the hicolor theme.
/// Returns the installed path, or None if the icon couldn't be resolved
/// (theme icon names are left as-is by the caller).
fn install_icon(icon_ref: &str, slug: &str, install_root: &Path) -> Option<PathBuf> {
    // Theme icon name (no slash, no extension) — nothing to install.
    if !icon_ref.contains('/') && !icon_ref.contains('.') {
        return None;
    }
    let src = if Path::new(icon_ref).is_absolute() {
        PathBuf::from(icon_ref)
    } else {
        // Relative to the .desktop file's dir, then to install root.
        let rel = Path::new(icon_ref);
        let under_root = install_root.join(rel);
        if under_root.exists() {
            under_root
        } else {
            return None;
        }
    };
    if !src.exists() {
        return None;
    }
    let ext = src
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let dest = UserDirs::new().icons.join(format!("dropin-{slug}{ext}"));
    std::fs::copy(&src, &dest).ok()?;
    Some(dest)
}

/// Generate a bare-bones entry when the app ships none.
pub fn generate_entry(app_name: &str, slug: &str, exec: &Path) -> Result<(PathBuf, Vec<PathBuf>)> {
    let entry = DesktopEntry {
        name: app_name.to_string(),
        exec: exec.display().to_string(),
        icon: None,
        comment: Some(format!("Installed with dropin ({})", slug)),
        categories: None,
        terminal: false,
    };
    let path = entry.write(slug)?;
    Ok((path.clone(), vec![path]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slugify;

    #[test]
    fn rewrite_exec_usr_paths() {
        let root = Path::new("/home/u/.local/opt/foo");
        assert_eq!(
            rewrite_exec("/usr/bin/foo --flag %F", root, false),
            "/home/u/.local/opt/foo/usr/bin/foo --flag %F"
        );
        assert_eq!(
            rewrite_exec("bin/foo", root, false),
            "/home/u/.local/opt/foo/bin/foo"
        );
    }

    #[test]
    fn rewrite_exec_appimage_apprun() {
        // The AppImage convention: Exec=AppRun means the image itself.
        let img = Path::new("/home/u/Applications/app-x86_64.AppImage");
        assert_eq!(
            rewrite_exec("AppRun", img, true),
            "/home/u/Applications/app-x86_64.AppImage"
        );
        assert_eq!(
            rewrite_exec("AppRun %F", img, true),
            "/home/u/Applications/app-x86_64.AppImage %F"
        );
        // Absolute paths still pass through.
        assert_eq!(rewrite_exec("/usr/bin/app", img, true), "/usr/bin/app");
    }

    #[test]
    fn parse_and_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dropin-desktop-{}", std::process::id()));
        crate::test_env::with_home(&dir, || {
            let src = dir.join("app.desktop");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                &src,
                "[Desktop Entry]\nType=Application\nName=Demo\nExec=/usr/bin/demo %F\nIcon=demo\n",
            )
            .unwrap();
            let map = parse_desktop_file(&src).unwrap();
            assert_eq!(map["Name"], "Demo");
            assert_eq!(map["Exec"], "/usr/bin/demo %F");
            let (path, _) =
                adopt_desktop_file(&src, "demo", "Demo", Path::new("/opt/root")).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("Exec=/opt/root/usr/bin/demo %F"));
            assert!(text.contains("X-Dropin-Managed=true"));
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Foo Bar 2.0!"), "foo-bar-2-0");
    }
}
