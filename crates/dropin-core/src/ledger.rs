//! The install ledger: every path dropin creates or moves is recorded,
//! so `dropin remove` can reverse it.

use crate::{Error, Result, UserDirs};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
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
        let conn = Connection::open(path)?;
        let ledger = Self { conn };
        ledger.init()?;
        Ok(ledger)
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
        Ok(())
    }

    pub fn record(
        &self,
        name: &str,
        format: &str,
        version: Option<&str>,
        source_path: &Path,
        source_sha256: Option<&str>,
        install_root: Option<&Path>,
        external: bool,
        files: &[PathBuf],
    ) -> Result<i64> {
        // Replace any previous install of the same name: the old files were
        // already removed (or the install is being overwritten).
        self.conn.execute("DELETE FROM installs WHERE name = ?1", params![name])?;
        self.conn.execute(
            "INSERT INTO installs (name, format, version, source_path, source_sha256, install_root, external)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                name,
                format,
                version,
                source_path.display().to_string(),
                source_sha256,
                install_root.map(|p| p.display().to_string()),
                external as i32,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        for f in files {
            self.conn.execute(
                "INSERT OR IGNORE INTO files (install_id, path) VALUES (?1, ?2)",
                params![id, f.display().to_string()],
            )?;
        }
        Ok(id)
    }

    pub fn get(&self, name: &str) -> Result<Install> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, format, version, source_path, source_sha256, install_root, external, created_at
             FROM installs WHERE name = ?1",
        )?;
        let mut rows = stmt.query(params![name])?;
        let row = rows.next()?.ok_or_else(|| Error::NotInstalled(name.to_string()))?;
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
            created_at: row.get(8)?,
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
            "SELECT id, name, format, version, source_path, source_sha256, install_root, external, created_at
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
                row.get::<_, String>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, name, format, version, source_path, source_sha256, install_root, external, created_at) =
                r?;
            out.push(Install {
                id,
                name,
                format,
                version,
                source_path,
                source_sha256,
                install_root,
                external: external != 0,
                created_at,
                files: self.files_for(id)?,
            });
        }
        Ok(out)
    }

    /// Remove the ledger rows for an install (call *after* deleting the files).
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
            .record(
                "demo",
                "AppImage",
                Some("1.0"),
                Path::new("/tmp/demo.AppImage"),
                None,
                None,
                false,
                &[PathBuf::from("/tmp/x.desktop")],
            )
            .unwrap();
        let got = ledger.get("demo").unwrap();
        assert_eq!(got.format, "AppImage");
        assert_eq!(got.files, vec!["/tmp/x.desktop".to_string()]);
        assert_eq!(ledger.list().unwrap().len(), 1);
        ledger.forget("demo").unwrap();
        assert!(ledger.get("demo").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
