//! Flatpak handler: `flatpak install --user` for .flatpakref/.flatpak.

use super::{InstallOptions, InstallOutcome, InstallPlan};
use crate::{detect, have, Error, Result};
use std::path::Path;

fn ref_id(src: &Path) -> Result<String> {
    let text = std::fs::read_to_string(src)?;
    text.lines()
        .find_map(|l| l.trim().strip_prefix("Name="))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| Error::Unsupported("flatpakref has no Name=".into()))
}

pub fn plan(src: &Path) -> Result<InstallPlan> {
    if !have("flatpak") {
        return Err(Error::Command("flatpak is not installed".into()));
    }
    let d = detect::detect(src)?;
    let id = match d.format {
        detect::Format::FlatpakRef => ref_id(src).unwrap_or_else(|_| d.name_hint.clone()),
        _ => d.name_hint.clone(),
    };
    Ok(InstallPlan {
        app_name: id.clone(),
        version: None,
        steps: vec![format!("flatpak install --user {id}")],
        needs_root: false,
        needs_confirmation: false,
    })
}

pub fn install(src: &Path, _opts: &InstallOptions) -> Result<InstallOutcome> {
    if !have("flatpak") {
        return Err(Error::Command("flatpak is not installed".into()));
    }
    let d = detect::detect(src)?;
    let s = src
        .to_str()
        .ok_or_else(|| Error::Unsupported("bad path".into()))?;
    crate::run("flatpak", &["install", "--user", "--noninteractive", s])?;
    // Re-read the id after install for the ledger.
    let id = match d.format {
        detect::Format::FlatpakRef => ref_id(src).unwrap_or_else(|_| d.name_hint.clone()),
        _ => d.name_hint.clone(),
    };
    Ok(InstallOutcome {
        name: crate::slugify(&id),
        version: None,
        install_root: None,
        paths: vec![],
        external: true, // flatpak owns the files; removal delegates to flatpak
        update_info: None,
    })
}

pub fn remove_external(name_or_id: &str) -> Result<()> {
    // The ledger stores the slug; try it as an app id directly.
    let id = name_or_id;
    match crate::run(
        "flatpak",
        &["uninstall", "--user", "--noninteractive", "-y", id],
    ) {
        Ok(_) => Ok(()),
        Err(e) => Err(Error::Command(format!("flatpak uninstall failed: {e}"))),
    }
}
