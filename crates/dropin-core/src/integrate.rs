//! File-manager integration: "Install as application" context-menu entries
//! for Dolphin, Nemo, Nautilus, and Thunar, plus the double-click MIME
//! registration. Everything is user-scoped and fully reversible.

use crate::{Result, UserDirs};
use std::path::PathBuf;

/// MIME types dropin-gui claims for double-click.
pub const GUI_MIMES: &[&str] = &[
    "application/x-iso9660-appimage",
    "application/vnd.debian.binary-package",
    "application/x-rpm",
    "application/x-redhat-package-manager",
    "application/vnd.flatpak.ref",
    "application/vnd.flatpak",
    "application/x-shellscript",
];

/// Archive types that get the context-menu entry (double-click stays with
/// the archive manager for these).
const ARCHIVE_MIMES: &[&str] = &[
    "application/gzip",
    "application/x-tar",
    "application/x-compressed-tar",
    "application/x-xz",
    "application/x-xz-compressed-tar",
    "application/x-bzip2",
    "application/x-bzip-compressed-tar",
    "application/x-zstd",
    "application/x-zstd-compressed-tar",
    "application/zip",
];

/// Archive globs for Thunar (which matches on filename, not MIME).
const ARCHIVE_GLOBS: &[&str] = &[
    "*.tar.gz",
    "*.tgz",
    "*.tar.xz",
    "*.txz",
    "*.tar.bz2",
    "*.tbz2",
    "*.tar.zst",
    "*.zip",
    "*.AppImage",
    "*.deb",
    "*.rpm",
    "*.flatpakref",
    "*.flatpak",
    "*.sh",
    "*.run",
    "*.bin",
];

fn gui_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("dropin-gui")))
        .unwrap_or_else(|| PathBuf::from("dropin-gui"))
}

fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::home_dir().join(".config"))
}

fn all_context_mimes() -> String {
    GUI_MIMES
        .iter()
        .chain(ARCHIVE_MIMES.iter())
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(";")
}

/// Write a file, creating parent dirs. Returns the path for the ledger-like
/// reporting (integrate is not ledger-tracked; unintegrate removes by path).
fn write_file(path: &PathBuf, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn remove_file(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// True when the MIME handler registration looks done (the marker file
/// `integrate(true)` writes exists). Used by the GUI's first-run prompt.
pub fn is_integrated() -> bool {
    let dirs = UserDirs::new();
    dirs.desktop_entries.join("dropin-gui.desktop").exists()
}

pub fn integrate(enable: bool) -> Result<()> {
    let dirs = UserDirs::new();
    dirs.ensure()?;
    if enable {
        mime_registration(&dirs)?;
        dolphin(true)?;
        nemo(true)?;
        nautilus(true)?;
        thunar(true)?;
        println!(
            "dropin will now handle double-clicked AppImages, .deb/.rpm packages,\n\
             flatpak refs, and installer scripts — and adds “Install as application”\n\
             to the right-click menu in Dolphin, Nemo, Nautilus, and Thunar\n\
             (archives included there)."
        );
    } else {
        mime_registration_remove(&dirs);
        dolphin(false)?;
        nemo(false)?;
        nautilus(false)?;
        thunar(false)?;
        println!("dropin file-manager integration removed.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Double-click MIME registration (M2)
// ---------------------------------------------------------------------------

fn mime_registration(dirs: &UserDirs) -> Result<()> {
    let desktop_path = dirs.desktop_entries.join("dropin-gui.desktop");
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=Install with dropin\n\
         Exec={} %f\nNoDisplay=true\nMimeType={};\nX-Dropin-Managed=true\n",
        gui_exe().display(),
        GUI_MIMES.join(";")
    );
    write_file(&desktop_path, &desktop)?;

    // Visible launcher for the manager window (the MIME handler above stays
    // hidden). This is how GUI-only users open dropin.
    let manager_path = dirs.desktop_entries.join("dropin.desktop");
    let manager = format!(
        "[Desktop Entry]\nType=Application\nName=dropin\n\
         Comment=Install and manage apps\nExec={} --manager\n\
         Icon=system-software-install\nCategories=System;PackageManager;\n\
         X-Dropin-Managed=true\n",
        gui_exe().display(),
    );
    write_file(&manager_path, &manager)?;

    let mimeapps = config_home().join("mimeapps.list");
    let mut text = std::fs::read_to_string(&mimeapps).unwrap_or_default();
    if !text.contains("[Default Applications]") {
        text.push_str("[Default Applications]\n");
    }
    for mime in GUI_MIMES {
        let line = format!("{mime}=dropin-gui.desktop");
        if !text.contains(&line) {
            text = text.replacen(
                "[Default Applications]\n",
                &format!("[Default Applications]\n{line}\n"),
                1,
            );
        }
    }
    write_file(&mimeapps, &text)?;

    let _ = std::process::Command::new("update-desktop-database")
        .arg(&dirs.desktop_entries)
        .output();
    Ok(())
}

fn mime_registration_remove(dirs: &UserDirs) {
    remove_file(&dirs.desktop_entries.join("dropin-gui.desktop"));
    remove_file(&dirs.desktop_entries.join("dropin.desktop"));
    // Leave mimeapps.list entries: harmless if the .desktop is gone, and
    // editing them out risks mangling user config. The NoDisplay entry
    // disappearing is enough.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&dirs.desktop_entries)
        .output();
}

// ---------------------------------------------------------------------------
// Dolphin (KDE) service menu
// ---------------------------------------------------------------------------

fn dolphin(enable: bool) -> Result<()> {
    let path = crate::home_dir().join(".local/share/kio/servicemenus/dropin-install.desktop");
    if !enable {
        remove_file(&path);
        return Ok(());
    }
    let content = format!(
        "[Desktop Entry]\n\
         Type=Service\n\
         X-KDE-ServiceTypes=KonqPopupMenu/Plugin\n\
         MimeType={};\n\
         Actions=InstallWithDropin;\n\
         X-KDE-Priority=TopLevel\n\
         X-Dropin-Managed=true\n\
         \n\
         [Desktop Action InstallWithDropin]\n\
         Name=Install as application\n\
         Icon=application-x-executable\n\
         Exec={} %f\n",
        all_context_mimes(),
        gui_exe().display()
    );
    write_file(&path, &content)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Nemo (Cinnamon) action
// ---------------------------------------------------------------------------

fn nemo(enable: bool) -> Result<()> {
    let path = crate::home_dir().join(".local/share/nemo/actions/dropin-install.nemo_action");
    if !enable {
        remove_file(&path);
        return Ok(());
    }
    let content = format!(
        "[Nemo Action]\n\
         Active=true\n\
         Name=Install as application\n\
         Comment=Install with dropin\n\
         Exec={} %F\n\
         Icon-Name=application-x-executable\n\
         Selection=any\n\
         Extensions=nodirs;\n\
         Mimetypes={};\n\
         X-Dropin-Managed=true\n",
        gui_exe().display(),
        all_context_mimes()
    );
    write_file(&path, &content)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Nautilus (GNOME) Python extension
// ---------------------------------------------------------------------------

fn nautilus(enable: bool) -> Result<()> {
    let path = crate::home_dir().join(".local/share/nautilus-python/extensions/dropin.py");
    if !enable {
        remove_file(&path);
        // Nautilus byte-compiles extensions on load; don't leave ours behind.
        let cache = path.parent().unwrap().join("__pycache__");
        let _ = std::fs::remove_dir_all(cache);
        return Ok(());
    }
    // MIME list baked in at install time; the GUI re-validates anyway.
    let mimes = all_context_mimes()
        .split(';')
        .map(|m| format!("    \"{m}\","))
        .collect::<Vec<_>>()
        .join("\n");
    let content = format!(
        "# Dropin Nautilus extension — “Install as application”.\n\
         # Managed by `dropin integrate` / `dropin unintegrate`.\n\
         # Requires the nautilus-python package (python3-nautilus).\n\
         from gi.repository import Nautilus, GObject\n\
         import subprocess\n\
         \n\
         GUI = \"{gui}\"\n\
         MIMES = {{\n\
         {mimes}\n\
         }}\n\
         \n\
         class DropinExtension(GObject.GObject, Nautilus.MenuProvider):\n\
         \x20   def get_file_items(self, files):\n\
         \x20       if len(files) != 1:\n\
         \x20           return []\n\
         \x20       f = files[0]\n\
         \x20       if f.is_directory() or f.get_mime_type() not in MIMES:\n\
         \x20           return []\n\
         \x20       item = Nautilus.MenuItem(\n\
         \x20           name=\"Dropin::Install\",\n\
         \x20           label=\"Install as application\",\n\
         \x20       )\n\
         \x20       item.connect(\"activate\", self.on_activate, f)\n\
         \x20       return [item]\n\
         \n\
         \x20   def on_activate(self, _menu, f):\n\
         \x20       path = f.get_location().get_path()\n\
         \x20       subprocess.Popen([GUI, path])\n",
        gui = gui_exe().display(),
        mimes = mimes,
    );
    write_file(&path, &content)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Thunar (Xfce) custom action (uca.xml merge)
// ---------------------------------------------------------------------------

const THUNAR_ACTION_ID: &str = "dropin-install-application";

fn thunar(enable: bool) -> Result<()> {
    let path = config_home().join("Thunar/uca.xml");
    let mut xml = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n".to_string()
    });

    // Remove any previous dropin action (idempotent re-install).
    xml = remove_thunar_action(&xml);

    if enable {
        let action = format!(
            "<action>\n\
             \x20<icon>application-x-executable</icon>\n\
             \x20<name>Install as application</name>\n\
             \x20<unique-id>{id}</unique-id>\n\
             \x20<command>{gui} %f</command>\n\
             \x20<description>Install with dropin</description>\n\
             \x20<patterns>{patterns}</patterns>\n\
             \x20<other-files/>\n\
             </action>\n",
            id = THUNAR_ACTION_ID,
            gui = gui_exe().display(),
            patterns = ARCHIVE_GLOBS.join(";"),
        );
        // Insert before </actions>; create the root if missing.
        if xml.contains("</actions>") {
            xml = xml.replacen("</actions>", &format!("{action}</actions>"), 1);
        } else {
            xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n{action}</actions>\n"
            );
        }
        // Safety backup before writing a merged file.
        let backup = path.with_extension("xml.dropin-bak");
        if path.exists() && !backup.exists() {
            let _ = std::fs::copy(&path, &backup);
        }
    }

    write_file(&path, &xml)?;
    Ok(())
}

/// Remove the <action> block whose <unique-id> is ours. Simple string scan —
/// avoids pulling in an XML parser for a 20-line job.
fn remove_thunar_action(xml: &str) -> String {
    let marker = format!("<unique-id>{THUNAR_ACTION_ID}</unique-id>");
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<action>") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find("</action>") {
            Some(rel_end) => {
                let end = start + rel_end + "</action>".len();
                let block = &rest[start..end];
                if !block.contains(&marker) {
                    out.push_str(block);
                }
                rest = &rest[end..];
            }
            None => {
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}
