//! User settings, stored as JSON under the data dir.
//!
//! Currently just the automatic update-check toggle for the GUI manager.
//! Best-effort: a missing or corrupt file falls back to defaults.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Check for updates automatically when the manager opens.
    pub auto_check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_check_updates: true,
        }
    }
}

fn path() -> PathBuf {
    crate::UserDirs::new().data.join("settings.json")
}

pub fn load() -> Settings {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(s: &Settings) -> Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(s).map_err(|e| Error::Command(e.to_string()))?;
    std::fs::write(p, text)?;
    Ok(())
}
