//! Leftover cleanup for `dropin remove --purge`.
//!
//! After the ledger-tracked files are gone, apps often leave residue in
//! `~/.config`, `~/.local/share` and `~/.cache`. This module finds candidate
//! leftovers with deliberately conservative heuristics and moves them to the
//! freedesktop trash — never deletes them outright.
//!
//! A directory is only a purge candidate when ALL of these hold:
//! - its name matches the app slug or menu name (case-insensitive,
//!   alphanumeric-normalized),
//! - it was created/modified at or after the install time (pre-existing
//!   directories are never touched),
//! - it lives directly under one of the three XDG base dirs (no recursion
//!   into unrelated trees).

use crate::ledger::Install;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct Leftover {
    pub path: PathBuf,
    pub what: String,
}

fn norm(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Parse SQLite `datetime('now')` ("YYYY-MM-DD HH:MM:SS", UTC) into a
/// SystemTime. Returns None when the format is unexpected — callers must
/// treat that as "do not purge" (fail closed).
fn parse_created_at(s: &str) -> Option<SystemTime> {
    let (date, time) = s.split_once(' ')?;
    let mut di = date.split('-');
    let (y, m, d): (i64, i64, i64) = (
        di.next()?.parse().ok()?,
        di.next()?.parse().ok()?,
        di.next()?.parse().ok()?,
    );
    let mut ti = time.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        ti.next()?.parse().ok()?,
        ti.next()?.parse().ok()?,
        ti.next()?.parse().ok()?,
    );
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs as u64))
}

/// Candidate names for an app's data dirs: the slug and the menu name.
fn candidate_names(inst: &Install) -> Vec<String> {
    let mut names = vec![norm(&inst.name)];
    // Also try the Name= from the managed .desktop file.
    for f in &inst.files {
        if !f.ends_with(".desktop") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(f) {
            for line in text.lines() {
                if let Some(v) = line.trim().strip_prefix("Name=") {
                    let n = norm(v.trim());
                    if !n.is_empty() && !names.contains(&n) {
                        names.push(n);
                    }
                    break;
                }
            }
        }
    }
    names
}

fn base_dirs() -> Vec<(&'static str, PathBuf)> {
    let home = crate::home_dir();
    vec![
        (".config", home.join(".config")),
        (".local/share", home.join(".local/share")),
        (".cache", home.join(".cache")),
    ]
}

/// Find leftover directories for an install. Read-only; deletes nothing.
pub fn find_leftovers(inst: &Install) -> Vec<Leftover> {
    let installed_at = match parse_created_at(&inst.created_at) {
        Some(t) => t,
        None => return Vec::new(), // fail closed
    };
    let names = candidate_names(inst);
    let mut out = Vec::new();
    for (kind, base) in base_dirs() {
        let entries = match std::fs::read_dir(&base) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || path.is_symlink() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            if !names.contains(&norm(&dir_name)) {
                continue;
            }
            // Only things created at/after the install: pre-existing dirs
            // (e.g. shared with a system package) are never candidates.
            let mtime = entry.metadata().and_then(|m| m.modified()).ok();
            match mtime {
                Some(t) if t >= installed_at => out.push(Leftover {
                    path,
                    what: format!("~/{kind}/{dir_name}"),
                }),
                _ => {}
            }
        }
    }
    out
}

/// Move leftovers to the freedesktop trash
/// (`~/.local/share/Trash/files/` + `.trashinfo`). Returns the trashed paths.
pub fn trash_leftovers(leftovers: &[Leftover]) -> crate::Result<Vec<PathBuf>> {
    let home = crate::home_dir();
    let files_dir = home.join(".local/share/Trash/files");
    let info_dir = home.join(".local/share/Trash/info");
    std::fs::create_dir_all(&files_dir)?;
    std::fs::create_dir_all(&info_dir)?;

    let mut trashed = Vec::new();
    for l in leftovers {
        let base = l
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        // Avoid collisions in the trash.
        let mut dest = files_dir.join(&base);
        let mut n = 1;
        while dest.exists() {
            n += 1;
            dest = files_dir.join(format!("{base}.{n}"));
        }
        // Same filesystem in practice; fall back to copy+delete.
        if std::fs::rename(&l.path, &dest).is_err() {
            copy_dir(&l.path, &dest)?;
            std::fs::remove_dir_all(&l.path)?;
        }
        let info_name = dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let deletion_date = chrono_stamp();
        std::fs::write(
            info_dir.join(format!("{info_name}.trashinfo")),
            format!(
                "[Trash Info]\nPath={}\nDeletionDate={}\n",
                l.path.display(),
                deletion_date
            ),
        )?;
        trashed.push(dest);
    }
    Ok(trashed)
}

fn chrono_stamp() -> String {
    // YYYY-MM-DDTHH:MM:SS without pulling in chrono.
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = now.div_euclid(86400);
    let secs = now.rem_euclid(86400);
    // civil_from_days (Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        if m <= 2 { y + 1 } else { y },
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() && !from.is_symlink() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Install;

    fn tmp_home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dropin-cleanup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for sub in [".config", ".local/share", ".cache"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        d
    }

    fn install(name: &str, created_at: &str) -> Install {
        Install {
            id: 1,
            name: name.into(),
            format: "AppImage".into(),
            version: None,
            source_path: "/tmp/x".into(),
            source_sha256: None,
            install_root: None,
            external: false,
            update_info: None,
            verified: false,
            created_at: created_at.into(),
            files: vec![],
        }
    }

    use crate::test_env::with_home;

    #[test]
    fn finds_fresh_leftover_dir() {
        let home = tmp_home("fresh");
        // created just now: mtime >= install time.
        std::fs::create_dir(home.join(".config").join("myapp")).unwrap();
        let inst = install("myapp", "2000-01-01 00:00:00");
        let found = with_home(&home, || find_leftovers(&inst));
        assert_eq!(found.len(), 1);
        assert!(found[0].what.contains("myapp"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn skips_preexisting_dirs() {
        let home = tmp_home("pre");
        let dir = home.join(".config").join("oldapp");
        std::fs::create_dir(&dir).unwrap();
        // Fake an old mtime by installing "in the future".
        let inst = install("oldapp", "2999-01-01 00:00:00");
        let found = with_home(&home, || find_leftovers(&inst));
        assert!(found.is_empty(), "pre-existing dir must not be purged");
        assert!(dir.exists(), "dir must be untouched");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn trash_moves_not_deletes() {
        let home = tmp_home("trash");
        let dir = home.join(".cache").join("myapp");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("blob"), b"data").unwrap();
        let inst = install("myapp", "2000-01-01 00:00:00");
        let (found, trashed) = with_home(&home, || {
            let found = find_leftovers(&inst);
            let trashed = trash_leftovers(&found).unwrap();
            (found, trashed)
        });
        assert_eq!(found.len(), 1);
        assert!(!dir.exists(), "original moved away");
        assert_eq!(trashed.len(), 1);
        assert!(trashed[0].join("blob").exists(), "contents preserved in trash");
        assert!(home.join(".local/share/Trash/info/myapp.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn bad_timestamp_fails_closed() {
        let home = tmp_home("badts");
        std::fs::create_dir(home.join(".config").join("myapp")).unwrap();
        let inst = install("myapp", "not-a-timestamp");
        let found = with_home(&home, || find_leftovers(&inst));
        assert!(found.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }
}
