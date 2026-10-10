//! The install ledger: every path dropin creates or moves is recorded,
//! so `dropin remove` can reverse it.

use crate::{Error, Result, UserDirs};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Install {
    pub id: i64,
    pub name: String,
    pub format: String,
    pub version: Option<String>,
    pub source_path: String,
    pub source_sha256: Option<String>,
    /// Top-level directory the app lives in (for tarball/deb rootless installs).
    pub install_root: Option<String>,
    /// True when installed outside dropin's tracking (system package manager,
    /// executed .run script). Removal is best-effort / delegated.
    pub external: bool,
    /// Where to check for updates (e.g. X-AppImage-UpdateInformation).
    pub update_info: Option<String>,
    /// True when a companion checksum verified at install time.
    pub verified: bool,
    /// GitHub repo ("owner/repo") tracked for release updates.
    pub github_repo: Option<String>,
    /// Pinned OpenPGP signing-key fingerprint (TOFU).
    pub signing_key: Option<String>,
    /// Per-app environment variables (JSON object, e.g. {"FOO": "bar"}).
    pub env_vars: Option<String>,
    pub created_at: String,
    pub files: Vec<String>,
}

pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    pub fn open() -> Result<Self> {
        let dirs = UserDirs::new();
        dirs.ensure()?;
        let path = dirs.data.join("ledger.db");
        // If the database is corrupted, back it up and start fresh rather
        // than refusing to run at all.
        if path.exists() {
            if let Err(e) = Self::check_integrity(&path) {
                let backup = dirs.data.join(format!(
                    "ledger.db.corrupt-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                ));
                let _ = std::fs::rename(&path, &backup);
                eprintln!(
                    "dropin: install database was corrupted ({e}); backed up to {} and starting fresh",
                    backup.display()
                );
            }
        }
        let conn = Connection::open(path)?;
        let ledger = Self { conn };
        ledger.init()?;
        Ok(ledger)
    }

    /// Quick integrity check: can we open and query the database?
    fn check_integrity(path: &std::path::Path) -> Result<()> {
        let conn = Connection::open(path)?;
        conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))?;
        Ok(())
    }

    /// Open a ledger at an explicit path (tests).
    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let ledger = Self { conn };
        ledger.init()?;
        Ok(ledger)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS installs (
                id            INTEGER PRIMARY KEY,
                name          TEXT NOT NULL UNIQUE,
                format        TEXT NOT NULL,
                version       TEXT,
                source_path   TEXT NOT NULL,
                source_sha256 TEXT,
                install_root  TEXT,
                external      INTEGER NOT NULL DEFAULT 0,
                created_at    TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS files (
                install_id INTEGER NOT NULL REFERENCES installs(id) ON DELETE CASCADE,
                path       TEXT NOT NULL,
                PRIMARY KEY (install_id, path)
             );",
        )?;
        // Migration (M4): where to check for updates, e.g. an AppImage's
        // X-AppImage-UpdateInformation string.
        // Migration (Phase 1): whether the install's checksum verified.
        let cols: Vec<String> = self
            .conn
            .prepare("PRAGMA table_info(installs)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<_, _>>()?;
        if !cols.iter().any(|c| c == "update_info") {
            self.conn
                .execute("ALTER TABLE installs ADD COLUMN update_info TEXT", [])?;
        }
        if !cols.iter().any(|c| c == "verified") {
            self.conn.execute(
                "ALTER TABLE installs ADD COLUMN verified INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        // Migration (Phase 4): GitHub repo tracked for release updates,
        // e.g. "owner/repo".
        if !cols.iter().any(|c| c == "github_repo") {
            self.conn
                .execute("ALTER TABLE installs ADD COLUMN github_repo TEXT", [])?;
        }
        // Migration (Phase 6): pinned OpenPGP signing-key fingerprint
        // (TOFU on first install; updates refuse a changed key).
        if !cols.iter().any(|c| c == "signing_key") {
            self.conn
                .execute("ALTER TABLE installs ADD COLUMN signing_key TEXT", [])?;
        }
        // Migration: per-app environment variables (JSON object).
        if !cols.iter().any(|c| c == "env_vars") {
            self.conn
                .execute("ALTER TABLE installs ADD COLUMN env_vars TEXT", [])?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewInstall {
    pub name: String,
    pub format: String,
    pub version: Option<String>,
    pub source_path: PathBuf,
    pub source_sha256: Option<String>,
    pub install_root: Option<PathBuf>,
    pub external: bool,
    pub update_info: Option<String>,
    pub verified: bool,
    pub github_repo: Option<String>,
    pub signing_key: Option<String>,
    pub env_vars: Option<String>,
    pub files: Vec<PathBuf>,
}

impl Ledger {
    pub fn record(&self, inst: &NewInstall) -> Result<i64> {
        // Replace any previous install of the same name: the old files were
        // already removed (or the install is being overwritten).
        self.conn
            .execute("DELETE FROM installs WHERE name = ?1", params![inst.name])?;
        self.conn.execute(
            "INSERT INTO installs (name, format, version, source_path, source_sha256, install_root, external, update_info, verified, github_repo, signing_key, env_vars)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                inst.name,
                inst.format,
                inst.version,
                inst.source_path.display().to_string(),
                inst.source_sha256,
                inst.install_root.as_ref().map(|p| p.display().to_string()),
                inst.external as i32,
                inst.update_info,
                inst.verified as i32,
                inst.github_repo,
                inst.signing_key,
                inst.env_vars,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        for f in &inst.files {
            self.conn.execute(
                "INSERT OR IGNORE INTO files (install_id, path) VALUES (?1, ?2)",
                params![id, f.display().to_string()],
            )?;
        }
        Ok(id)
    }

    /// Update the pinned signing key (used when a signature is found on
    /// update and none was pinned, i.e. TOFU on a later version).
    pub fn set_signing_key(&self, name: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE installs SET signing_key = ?1 WHERE name = ?2",
            params![key, name],
        )?;
        Ok(())
    }

    /// Set per-app environment variables (JSON object string).
    pub fn set_env_vars(&self, name: &str, json: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE installs SET env_vars = ?1 WHERE name = ?2",
            params![json, name],
        )?;
        Ok(())
    }

    /// Get per-app environment variables as a map.
    pub fn env_vars(&self, name: &str) -> Result<std::collections::BTreeMap<String, String>> {
        let inst = self.get(name)?;
        match inst.env_vars {
            Some(json) => serde_json::from_str(&json)
                .map_err(|e| crate::Error::Unsupported(format!("bad env vars for {name}: {e}"))),
            None => Ok(std::collections::BTreeMap::new()),
        }
    }

    pub fn get(&self, name: &str) -> Result<Install> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, format, version, source_path, source_sha256, install_root, external, update_info, verified, github_repo, signing_key, env_vars, created_at
             FROM installs WHERE name = ?1",
        )?;
        let mut rows = stmt.query(params![name])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotInstalled(name.to_string()))?;
        let id: i64 = row.get(0)?;
        let files = self.files_for(id)?;
        Ok(Install {
            id,
            name: row.get(1)?,
            format: row.get(2)?,
            version: row.get(3)?,
            source_path: row.get(4)?,
            source_sha256: row.get(5)?,
            install_root: row.get(6)?,
            external: row.get::<_, i32>(7)? != 0,
            update_info: row.get(8)?,
            verified: row.get::<_, i32>(9)? != 0,
            github_repo: row.get(10)?,
            signing_key: row.get(11)?,
            env_vars: row.get(12)?,
            created_at: row.get(13)?,
            files,
        })
    }

    fn files_for(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files WHERE install_id = ?1 ORDER BY path")?;
        let paths = stmt
            .query_map(params![id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(paths)
    }

    pub fn list(&self) -> Result<Vec<Install>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, format, version, source_path, source_sha256, install_root, external, update_info, verified, github_repo, signing_key, env_vars, created_at
             FROM installs ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, i32>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, i32>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, String>(13)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (
                id,
                name,
                format,
                version,
                source_path,
                source_sha256,
                install_root,
                external,
                update_info,
                verified,
                github_repo,
                signing_key,
                env_vars,
                created_at,
            ) = r?;
            out.push(Install {
                id,
                name,
                format,
                version,
                source_path,
                source_sha256,
                install_root,
                external: external != 0,
                update_info,
                verified: verified != 0,
                github_repo,
                signing_key,
                env_vars,
                created_at,
                files: self.files_for(id)?,
            });
        }
        Ok(out)
    }

    /// Set (or clear) the GitHub repo tracked for release updates.
    pub fn set_github_repo(&self, name: &str, repo: Option<&str>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE installs SET github_repo = ?1 WHERE name = ?2",
            params![repo, name],
        )?;
        if n == 0 {
            return Err(Error::NotInstalled(name.to_string()));
        }
        Ok(())
    }

    /// Rename a ledger entry, keeping its files and metadata. Used when an
    /// update's detected app name differs from the name the user knows —
    /// the entry keeps the original name so no duplicate appears.
    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE installs SET name = ?1 WHERE name = ?2",
            params![new, old],
        )?;
        if n == 0 {
            return Err(Error::NotInstalled(old.to_string()));
        }
        Ok(())
    }

    /// Clear the recorded version (e.g. after a rollback: the restored
    /// snapshot's version is unknown, so the next update check re-syncs).
    pub fn clear_version(&self, name: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE installs SET version = NULL WHERE name = ?1",
            params![name],
        )?;
        if n == 0 {
            return Err(Error::NotInstalled(name.to_string()));
        }
        Ok(())
    }

    /// Record the installed version (e.g. the GitHub tag just installed).
    pub fn set_version(&self, name: &str, version: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE installs SET version = ?1 WHERE name = ?2",
            params![version, name],
        )?;
        if n == 0 {
            return Err(Error::NotInstalled(name.to_string()));
        }
        Ok(())
    }

    /// Remove the ledger rows for an install (call *after* deleting the files).
    /// Swap one tracked file path for another (used when migrating an
    /// install's on-disk layout, e.g. flat AppImage -> generation dir).
    pub fn replace_file(&self, name: &str, old: &str, new: &str) -> Result<()> {
        let id: i64 = self.conn.query_row(
            "SELECT id FROM installs WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "DELETE FROM files WHERE install_id = ?1 AND path = ?2",
            params![id, old],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO files (install_id, path) VALUES (?1, ?2)",
            params![id, new],
        )?;
        Ok(())
    }

    pub fn forget(&self, name: &str) -> Result<()> {
        let n = self
            .conn
            .execute("DELETE FROM installs WHERE name = ?1", params![name])?;
        if n == 0 {
            return Err(Error::NotInstalled(name.to_string()));
        }
        Ok(())
    }
}

/// Delete a recorded path: files/symlinks unlinked, dirs removed recursively.
/// Missing paths are fine (idempotent removal).
pub fn remove_path(path: &Path) -> Result<()> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    if path.is_dir() && !path.is_symlink() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_list_forget_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dropin-ledger-{}", std::process::id()));
        let ledger = Ledger::open_at(&dir.join("ledger.db")).unwrap();
        ledger
            .record(&NewInstall {
                name: "demo".into(),
                format: "AppImage".into(),
                version: Some("1.0".into()),
                source_path: PathBuf::from("/tmp/demo.AppImage"),
                files: vec![PathBuf::from("/tmp/x.desktop")],
                ..Default::default()
            })
            .unwrap();
        let got = ledger.get("demo").unwrap();
        assert_eq!(got.format, "AppImage");
        assert_eq!(got.files, vec!["/tmp/x.desktop".to_string()]);
        assert_eq!(ledger.list().unwrap().len(), 1);
        ledger.forget("demo").unwrap();
        assert!(ledger.get("demo").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_keeps_files_and_metadata() {
        let dir = std::env::temp_dir().join(format!("dropin-ledger-rn-{}", std::process::id()));
        let ledger = Ledger::open_at(&dir.join("ledger.db")).unwrap();
        ledger
            .record(&NewInstall {
                name: "newname".into(),
                format: "AppImage".into(),
                version: Some("2.0".into()),
                source_path: PathBuf::from("/tmp/new.AppImage"),
                github_repo: Some("owner/repo".into()),
                files: vec![PathBuf::from("/tmp/new.desktop")],
                ..Default::default()
            })
            .unwrap();
        ledger.rename("newname", "oldname").unwrap();
        assert!(ledger.get("newname").is_err());
        let got = ledger.get("oldname").unwrap();
        assert_eq!(got.version.as_deref(), Some("2.0"));
        assert_eq!(got.github_repo.as_deref(), Some("owner/repo"));
        assert_eq!(got.files, vec!["/tmp/new.desktop".to_string()]);
        assert!(ledger.rename("missing", "x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migrates_pre_m4_database() {
        let dir = std::env::temp_dir().join(format!("dropin-mig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("ledger.db");
        // Simulate a pre-M4 database (no update_info column).
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE installs (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
              format TEXT NOT NULL, version TEXT, source_path TEXT NOT NULL,
              source_sha256 TEXT, install_root TEXT, external INTEGER NOT NULL DEFAULT 0,
              created_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE files (install_id INTEGER NOT NULL, path TEXT NOT NULL,
              PRIMARY KEY (install_id, path));
             INSERT INTO installs (name, format, source_path) VALUES ('old','AppImage','/tmp/x');",
        )
        .unwrap();
        drop(conn);
        // Opening with the new code must migrate and preserve the row.
        let ledger = Ledger::open_at(&db_path).unwrap();
        let got = ledger.get("old").unwrap();
        assert_eq!(got.format, "AppImage");
        assert_eq!(got.update_info, None);
        // And new records can store update_info.
        ledger
            .record(&NewInstall {
                name: "new".into(),
                format: "AppImage".into(),
                source_path: PathBuf::from("/tmp/y"),
                update_info: Some("gh-releases-zsync|a|b|c".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            ledger.get("new").unwrap().update_info.as_deref(),
            Some("gh-releases-zsync|a|b|c")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
