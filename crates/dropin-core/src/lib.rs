//! dropin-core: format detection, install handlers, and the install ledger.
//!
//! Everything here is fully offline. Installs default to user scope
//! (no root required).

pub mod adopt;
pub mod cleanup;
pub mod desktop;
pub mod detect;
pub mod exe_icon;
pub mod github;
pub mod handlers;
pub mod health;
pub mod install;
pub mod ledger;
pub mod settings;
pub mod signatures;
pub mod verify;
pub mod zsync;

/// Test-only helpers for tests that redirect HOME/DROPIN_HOME.
///
/// Environment variables are process-global, and Rust runs tests in parallel
/// threads: every test that mutates them MUST go through this module, or
/// tests corrupt each other flakily.
#[cfg(test)]
pub(crate) mod test_env {
    use std::path::Path;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` with `HOME` and `DROPIN_HOME` pointed at `home`, restoring
    /// both afterwards. Serialized against all other users of this helper.
    pub(crate) fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = LOCK.lock().unwrap();
        let old_dropin = std::env::var("DROPIN_HOME").ok();
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("DROPIN_HOME", home);
        std::env::set_var("HOME", home);
        let result = f();
        match old_dropin {
            Some(v) => std::env::set_var("DROPIN_HOME", v),
            None => std::env::remove_var("DROPIN_HOME"),
        }
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        result
    }
}

use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("command failed: {0}")]
    Command(String),
    #[error("unsupported or unrecognized file: {0}")]
    Unsupported(String),
    #[error("refused: {0}")]
    Refused(String),
    #[error("not installed: {0}")]
    NotInstalled(String),
    #[error("zsync: {0}")]
    Zsync(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Home directory, or an override for tests via `DROPIN_HOME`.
pub fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("DROPIN_HOME") {
        return PathBuf::from(h);
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// XDG-ish user locations dropin manages.
pub struct UserDirs {
    pub applications: PathBuf,    // ~/Applications (AppImages live here)
    pub opt: PathBuf,             // ~/.local/opt/<app>
    pub bin: PathBuf,             // ~/.local/bin
    pub desktop_entries: PathBuf, // ~/.local/share/applications
    pub icons: PathBuf,           // ~/.local/share/icons/hicolor/256x256/apps
    pub data: PathBuf,            // ~/.local/share/dropin (ledger lives here)
}

impl UserDirs {
    pub fn new() -> Self {
        let home = home_dir();
        Self {
            applications: home.join("Applications"),
            opt: home.join(".local/opt"),
            bin: home.join(".local/bin"),
            desktop_entries: home.join(".local/share/applications"),
            icons: home.join(".local/share/icons/hicolor/256x256/apps"),
            data: home.join(".local/share/dropin"),
        }
    }

    /// Create all managed directories. Idempotent.
    pub fn ensure(&self) -> Result<()> {
        for d in [
            &self.applications,
            &self.opt,
            &self.bin,
            &self.desktop_entries,
            &self.icons,
            &self.data,
        ] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}

impl Default for UserDirs {
    fn default() -> Self {
        Self::new()
    }
}

/// Turn "Foo Bar_1.2.3-x86_64" into "foo-bar".
pub fn slugify(name: &str) -> String {
    let mut slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    slug.trim_matches('-').to_string()
}

/// Strip common version/arch/download suffixes to guess an app name:
/// "vesktop-1.5.9-x86_64" -> "vesktop", "foo_2.0_amd64" -> "foo".
pub fn clean_name(stem: &str) -> String {
    let mut s = stem.to_string();
    // Strip a trailing "(arch)" parenthetical: "My App (x86_64)" -> "My App".
    // Do it once up front; the loop below handles the rest.
    if let Some(open) = s.rfind('(') {
        if s.ends_with(')') {
            let inner = s[open + 1..s.len() - 1].to_lowercase();
            if ["x86_64", "amd64", "aarch64", "arm64", "x64", "i386", "i686"]
                .contains(&inner.as_str())
            {
                s.truncate(open);
                s = s.trim_end().to_string();
            }
        }
    }
    // Repeatedly strip trailing version/arch tokens.
    loop {
        let lower = s.to_lowercase();
        let cut = [
            "-x86_64",
            "-amd64",
            "-aarch64",
            "-arm64",
            "_x86_64",
            "_amd64",
            "-x64",
            "-linux",
            "-gnu",
            ".x86_64",
            "-portable",
        ]
        .iter()
        .find_map(|tok| lower.strip_suffix(tok).map(|_| s.len() - tok.len()));
        // Also strip trailing version numbers like -1.2.3, _2.0, v3.1
        let ver_cut = {
            let bytes = s.as_bytes();
            let mut i = bytes.len();
            while i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.') {
                i -= 1;
            }
            if i < bytes.len()
                && i > 0
                && (bytes[i - 1] == b'-'
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'v'
                    || bytes[i - 1] == b'V')
            {
                // make sure there is at least one digit in the stripped part
                let tail = &s[i..];
                if tail.chars().any(|c| c.is_ascii_digit()) {
                    Some(i - 1)
                } else {
                    None
                }
            } else {
                None
            }
        };
        match (cut, ver_cut) {
            (Some(c), Some(v)) => s.truncate(c.min(v)),
            (Some(c), None) => s.truncate(c),
            (None, Some(v)) => s.truncate(v),
            (None, None) => break,
        }
        s = s.trim_end_matches(['-', '_', '.']).to_string();
        if s.is_empty() {
            return stem.to_string();
        }
    }
    // "app-1.2.3" edge: the loop above handles it; final tidy.
    s.trim_end_matches(['-', '_', '.']).to_string()
}

/// File stem without the final extension: "foo-1.0.tar" for "foo-1.0.tar.gz".
pub fn stem_of(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Strip compound archive extensions first.
    for ext in [
        ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tar.zst",
    ] {
        if let Some(s) = name.strip_suffix(ext) {
            return s.to_string();
        }
    }
    Path::new(&name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or(name)
}

/// Run a command, returning stdout on success or a rich error.
pub fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| Error::Command(format!("failed to run `{cmd}`: {e}")))?;
    if !out.status.success() {
        return Err(Error::Command(format!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Which command exists on PATH?
pub fn have(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("command -v {cmd} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A unique temp dir path that won't collide between parallel processes.
/// Includes PID and nanosecond timestamp.
pub fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}
