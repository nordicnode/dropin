//! AppImage handler: move to ~/Applications, integrate into the menu.

use super::{InstallOptions, InstallOutcome, InstallPlan};
use crate::desktop::{adopt_desktop_file, generate_entry, parse_desktop_file};
use crate::{detect, slugify, Error, Result, UserDirs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn plan(src: &Path) -> Result<InstallPlan> {
    let d = detect::detect(src)?;
    Ok(InstallPlan {
        app_name: d.name_hint.clone(),
        version: None,
        steps: vec![
            format!("Move to ~/Applications/{}", src.file_name().unwrap().to_string_lossy()),
            "Make executable".into(),
            "Extract embedded icon and menu entry".into(),
            "Add to your application menu".into(),
        ],
        needs_root: false,
        needs_confirmation: false,
    })
}

pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    let d = detect::detect(src)?;
    if d.format != detect::Format::AppImage {
        return Err(Error::Unsupported(format!(
            "not an AppImage: {}",
            src.display()
        )));
    }
    let dirs = UserDirs::new();
    dirs.ensure()?;

    let file_name = src
        .file_name()
        .ok_or_else(|| Error::Unsupported("bad file name".into()))?;
    let dest: PathBuf = dirs.applications.join(file_name);
    if dest.exists() {
        return Err(Error::Refused(format!(
            "{} already exists in ~/Applications",
            file_name.to_string_lossy()
        )));
    }

    if opts.keep_source {
        std::fs::copy(src, &dest)?;
    } else {
        std::fs::rename(src, &dest)?;
    }
    let mut perms = std::fs::metadata(&dest)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&dest, perms)?;

    let mut paths = vec![dest.clone()];
    let slug = slugify(&d.name_hint);
    let mut update_info: Option<String> = None;

    // Extract the embedded .desktop file + icon for menu integration.
    // `--appimage-extract` works without FUSE (type 2 images).
    match extract_metadata(&dest) {
        Ok(meta) => {
            // Capture the embedded update channel for `dropin update`.
            if let Some(df) = &meta.desktop_file {
                update_info = parse_desktop_file(df)
                    .ok()
                    .and_then(|m| m.get("X-AppImage-UpdateInformation").cloned());
            }
            let (entry_path, mut extra) = match meta.desktop_file {
                Some(df) => adopt_desktop_file(&df, &slug, &d.name_hint, &dest)?,
                None => generate_entry(&d.name_hint, &slug, &dest)?,
            };
            let _ = entry_path;
            paths.append(&mut extra);
            let _ = std::fs::remove_dir_all(meta.scratch);
        }
        Err(e) => {
            // Integration is best-effort: the AppImage itself still works.
            eprintln!("warning: could not extract AppImage metadata ({e}); using a generic menu entry");
            let (_, mut extra) = generate_entry(&d.name_hint, &slug, &dest)?;
            paths.append(&mut extra);
        }
    }

    Ok(InstallOutcome {
        name: slug,
        version: None,
        install_root: None,
        paths,
        external: false,
        update_info,
    })
}

struct ExtractedMeta {
    desktop_file: Option<PathBuf>,
    scratch: PathBuf,
}

/// Run `<appimage> --appimage-extract` in a scratch dir and locate the
/// top-level .desktop file.
fn extract_metadata(appimage: &Path) -> Result<ExtractedMeta> {
    let scratch = std::env::temp_dir().join(format!(
        "dropin-ai-{}-{}",
        std::process::id(),
        appimage
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)?;
    }
    std::fs::create_dir_all(&scratch)?;
    let status = std::process::Command::new(appimage)
        .arg("--appimage-extract")
        .current_dir(&scratch)
        .output()
        .map_err(|e| Error::Command(format!("--appimage-extract failed: {e}")))?;
    if !status.status.success() {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(Error::Command(
            "--appimage-extract exited non-zero".into(),
        ));
    }
    let squash = scratch.join("squashfs-root");
    let desktop_file = std::fs::read_dir(&squash)
        .ok()
        .and_then(|entries| {
            entries.flatten().find(|e| {
                e.path().extension().map(|x| x == "desktop").unwrap_or(false)
            })
        })
        .map(|e| e.path())
        .filter(|p| parse_desktop_file(p).is_ok());
    Ok(ExtractedMeta {
        desktop_file,
        scratch,
    })
}
