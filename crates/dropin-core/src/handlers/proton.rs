//! Proton/GE-Proton support via umu-launcher.
//!
//! For games, plain Wine isn't enough. GE-Proton (GloriousEggroll's build)
//! adds gaming patches, media codecs, FSR, and per-game fixes. But Proton
//! must run inside Valve's container runtime — running it directly breaks.
//! umu-launcher recreates that runtime without needing Steam.
//!
//! protonfixes is automatic: when we pass a GAMEID to umu-run, it looks up
//! the game in the umu-database and applies per-game fixes (winetricks verbs,
//! env vars, DLL overrides) before launch. Zero user involvement.
//!
//! Flow:
//! 1. Bootstrap umu-launcher (pip install or system package)
//! 2. Resolve the game's umu-ID from the umu-database API (cached)
//! 3. Run via: GAMEID=umu-<id> PROTONPATH="GE-Proton" umu-run <exe>
//!    umu auto-downloads GE-Proton and the Steam Runtime on first run.

use crate::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Where umu-launcher and its data live.
pub fn umu_home() -> PathBuf {
    crate::home_dir().join(".local/share/dropin/umu")
}

/// True when umu-launcher is available.
pub fn have_umu() -> bool {
    which("umu-run").is_ok()
}

fn which(name: &str) -> Result<PathBuf> {
    let out = crate::run("which", &[name])?;
    let path = out.trim();
    if path.is_empty() {
        return Err(Error::Command(format!("{name} not found")));
    }
    Ok(PathBuf::from(path))
}

/// Install umu-launcher automatically.
/// Tries: system package manager -> pipx -> pip --user
pub fn bootstrap_umu(progress: &dyn Fn(&str)) -> Result<PathBuf> {
    if let Ok(path) = which("umu-run") {
        return Ok(path);
    }

    progress("Installing umu-launcher (enables Proton for games)…");

    // Try distro package first.
    let distro = super::wine_setup::distro_id();
    let pkg_result = match distro.as_str() {
        d if ["debian", "ubuntu", "linuxmint", "pop"].contains(&d) => {
            try_pkg_install("apt", &["install", "-y", "umu-launcher"], progress)
        }
        d if ["fedora", "rhel"].contains(&d) => {
            try_pkg_install("dnf", &["install", "-y", "umu-launcher"], progress)
        }
        d if ["arch", "manjaro", "endeavouros"].contains(&d) => {
            try_pkg_install("pacman", &["-S", "--noconfirm", "umu-launcher"], progress)
        }
        _ => Err(Error::Command("no distro package".into())),
    };

    if pkg_result.is_ok() {
        if let Ok(path) = which("umu-run") {
            progress("umu-launcher installed.");
            return Ok(path);
        }
    }

    // Fall back to pip.
    progress("Installing umu-launcher via pip…");
    for pip in ["pipx", "pip3", "pip"] {
        let args: Vec<&str> = if pip == "pipx" {
            vec!["install", "umu-launcher"]
        } else {
            vec!["install", "--user", "umu-launcher"]
        };
        let status = std::process::Command::new(pip)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if status.map(|s| s.success()).unwrap_or(false) {
            if let Ok(path) = which("umu-run") {
                progress("umu-launcher installed.");
                return Ok(path);
            }
            // pip --user puts it in ~/.local/bin
            let local_bin = crate::home_dir().join(".local/bin/umu-run");
            if local_bin.exists() {
                progress("umu-launcher installed.");
                return Ok(local_bin);
            }
        }
    }

    Err(Error::Command(
        "couldn't install umu-launcher automatically. Install it manually: \
         `pip install umu-launcher` or via your distro's package manager."
            .into(),
    ))
}

fn try_pkg_install(pkg_cmd: &str, args: &[&str], _progress: &dyn Fn(&str)) -> Result<()> {
    let escalate = if crate::have("pkexec") {
        "pkexec"
    } else {
        "sudo"
    };
    let status = std::process::Command::new(escalate)
        .arg(pkg_cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| Error::Command(format!("couldn't run {escalate}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Command(format!("{pkg_cmd} install failed")))
    }
}

// ---------------------------------------------------------------------------
// umu-database GAMEID resolution
// ---------------------------------------------------------------------------

/// Cache for GAMEID lookups: title -> umu_id.
fn gameid_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a game title to its umu-ID via the umu-database API.
/// Returns e.g. "umu-12345". Falls back to "umu-default" on any failure.
/// Results are cached in-memory for the session.
pub fn resolve_gameid(title: &str) -> String {
    let key = title.to_lowercase();
    if let Ok(cache) = gameid_cache().lock() {
        if let Some(id) = cache.get(&key) {
            return id.clone();
        }
    }

    let resolved = query_umu_database(title).unwrap_or_else(|| "umu-default".into());

    if let Ok(mut cache) = gameid_cache().lock() {
        cache.insert(key, resolved.clone());
    }
    resolved
}

/// Query https://umu.openwinecomponents.org/umu_api.php for a game title.
/// The API returns JSON with umu_id entries.
fn query_umu_database(title: &str) -> Option<String> {
    // The API supports title search. Keep it simple and fast.
    let url = format!(
        "https://umu.openwinecomponents.org/umu_api.php?title={}",
        urlencode(title)
    );
    let out = std::process::Command::new("curl")
        .args(["-sL", "--max-time", "10", &url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Look for "umu_id":"umu-12345" in the JSON.
    for line in text.lines() {
        if let Some(idx) = line.find("\"umu_id\"") {
            let rest = &line[idx..];
            if let Some(start) = rest.find("\"umu-") {
                let id_part = &rest[start + 1..];
                if let Some(end) = id_part.find('"') {
                    let id = &id_part[..end];
                    if id.starts_with("umu-") && id.len() > 4 {
                        return Some(id.to_string());
                    }
                }
            }
        }
    }
    // Also try a simpler pattern: "umu-12345" anywhere.
    for word in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-') {
        if word.starts_with("umu-")
            && word.len() > 4
            && word[4..].chars().all(|c| c.is_ascii_digit())
        {
            return Some(word.to_string());
        }
    }
    None
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Running games via umu + GE-Proton
// ---------------------------------------------------------------------------

/// The runner to use for a Windows app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// Plain Wine: best for applications.
    Wine,
    /// GE-Proton via umu: best for games.
    Proton,
}

impl Runner {
    pub fn label(self) -> &'static str {
        match self {
            Runner::Wine => "Wine",
            Runner::Proton => "GE-Proton",
        }
    }
}

/// Heuristic: is this likely a game?
/// Checks filename keywords, and common game installer patterns.
pub fn likely_game(name: &str, path: &Path) -> bool {
    let lower = name.to_lowercase();
    let path_lower = path.to_string_lossy().to_lowercase();

    // Obvious game keywords.
    for kw in [
        "game",
        "play",
        "gog",
        "steam",
        "epic",
        "ubisoft",
        "ea ",
        "setup_game",
        "gog-game",
    ] {
        if lower.contains(kw) || path_lower.contains(kw) {
            return true;
        }
    }
    // GOG installers are always games.
    if path_lower.contains("gog") || lower.starts_with("setup_") {
        return true;
    }
    // Large .exes are more likely games than utilities.
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > 500_000_000 {
            return true;
        }
    }
    false
}

/// Run a Windows executable via umu + GE-Proton.
/// `prefix` is the Wine prefix, `exe` is the executable to run.
/// protonfixes applies automatically based on GAMEID.
pub fn run_with_proton(
    prefix: &Path,
    exe: &Path,
    game_title: &str,
    extra_env: &HashMap<String, String>,
) -> Result<()> {
    let umu = which("umu-run").map_err(|_| {
        Error::Command("umu-launcher isn't installed. Run `dropin setup-proton` first.".into())
    })?;

    let gameid = resolve_gameid(game_title);

    // Capture output to a log file so we can diagnose crashes.
    let log_path = prefix.join("proton-last-run.log");
    let stdout_log = std::fs::File::create(&log_path).ok();

    let mut cmd = std::process::Command::new(&umu);
    cmd.env("WINEPREFIX", prefix);
    cmd.env("GAMEID", &gameid);
    cmd.env("STORE", "none");
    // "GE-Proton" tells umu to auto-download the latest GE-Proton build.
    cmd.env("PROTONPATH", "GE-Proton");
    // Performance env vars.
    cmd.env("DXVK_HUD", "0");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.arg(exe);
    cmd.stdin(std::process::Stdio::null());

    // Stdout goes to the log file (or inherit if we can't create it).
    // Stderr is piped so we can diagnose failures.
    match stdout_log {
        Some(f) => {
            cmd.stdout(std::process::Stdio::from(f));
        }
        None => {
            cmd.stdout(std::process::Stdio::inherit());
        }
    }
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Command(format!("umu-run failed to start: {e}")))?;

    // Drain stderr in a background thread to avoid pipe deadlock on
    // long-running games with verbose output. We keep the text for
    // diagnosing crashes.
    let stderr_text = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stderr_clone = stderr_text.clone();
    let stderr_handle = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = String::new();
            let _ = reader.read_to_string(&mut buf);
            if let Ok(mut guard) = stderr_clone.lock() {
                *guard = buf;
            }
        })
    });

    let status = child
        .wait()
        .map_err(|e| Error::Command(format!("umu-run failed: {e}")))?;

    // Collect the stderr text (thread has finished since the process exited).
    let stderr_str = stderr_handle
        .map(|h| {
            let _ = h.join();
            stderr_text.lock().map(|g| g.clone()).unwrap_or_default()
        })
        .unwrap_or_default();

    // Append stderr to the log file for completeness.
    if !stderr_str.is_empty() {
        use std::io::Write;
        if let Ok(mut log) = std::fs::OpenOptions::new().append(true).open(&log_path) {
            let _ = writeln!(log, "\n--- stderr ---\n{stderr_str}");
        }
    }

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        // Diagnose common failure modes from the captured output.
        let hint = diagnose_proton_failure(&stderr_str, code);
        return Err(Error::Command(format!(
            "the game exited with status {code}{hint}\n\nFull log: {}",
            log_path.display()
        )));
    }
    Ok(())
}

/// Analyze Proton/game output for known failure patterns and suggest fixes.
fn diagnose_proton_failure(stderr: &str, code: i32) -> String {
    // GLX/OpenGL context creation failure — broken graphics drivers.
    if stderr.contains("X_GLXCreateContext")
        || stderr.contains("BadValue")
        || stderr.contains("failed to create dri2 screen")
    {
        return "\n\nThis looks like a graphics driver problem: the game couldn't create an OpenGL context. \
            If you have an NVIDIA GPU, make sure the proprietary drivers are installed (not just Mesa). \
            You can also try launching with software rendering as a test.".into();
    }
    // Vulkan/DXVK issues.
    if stderr.contains("vulkan")
        && (stderr.contains("failed") || stderr.contains("not found") || stderr.contains("ERROR"))
    {
        return "\n\nVulkan isn't working properly. Try installing Vulkan drivers for your GPU, \
            or set PROTON_USE_WINED3D=1 to use Wine's OpenGL renderer instead of DXVK."
            .into();
    }
    // Missing prefix or corrupted install.
    if stderr.contains("unable to use parent for game drive") {
        return "\n\nProton had trouble setting up the game drive. This can happen with unusual install paths. \
            Try removing and reinstalling the game.".into();
    }
    // Generic: point to the log.
    if code == 53 {
        return "\n\nThe game crashed on startup (status 53). Check the log file above for details, \
            or try verifying the game files.".into();
    }
    String::new()
}

/// Build the launch command for a Proton game (for .desktop entries).
/// Returns (env_vars, command_argv).
pub fn proton_launch_cmd(
    prefix: &Path,
    exe: &Path,
    game_title: &str,
) -> (Vec<(String, String)>, Vec<String>) {
    let gameid = resolve_gameid(game_title);
    let env = vec![
        ("WINEPREFIX".into(), prefix.to_string_lossy().into_owned()),
        ("GAMEID".into(), gameid),
        ("STORE".into(), "none".into()),
        ("PROTONPATH".into(), "GE-Proton".into()),
    ];
    let argv = vec!["umu-run".into(), exe.to_string_lossy().into_owned()];
    (env, argv)
}

/// Set up Proton support: bootstrap umu-launcher.
/// GE-Proton and the Steam Runtime are auto-downloaded by umu on first use.
pub fn setup_proton(progress: &dyn Fn(&str)) -> Result<PathBuf> {
    let umu = bootstrap_umu(progress)?;
    progress(
        "Proton support is ready. GE-Proton will download automatically on first game launch.",
    );
    Ok(umu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_heuristic() {
        assert!(likely_game(
            "setup_witcher3",
            Path::new("/tmp/setup_the_witcher_3.exe")
        ));
        assert!(likely_game(
            "game-installer",
            Path::new("/tmp/gog-game.exe")
        ));
        assert!(!likely_game("notepad", Path::new("/tmp/notepad.exe")));
    }

    #[test]
    fn urlencode_works() {
        assert_eq!(urlencode("The Witcher 3"), "The+Witcher+3");
    }

    #[test]
    fn diagnose_detects_glx_failure() {
        let stderr = "X Error of failed request: BadValue\nMajor opcode: 150 (GLX)\nMinor opcode: 3 (X_GLXCreateContext)";
        let hint = diagnose_proton_failure(stderr, 53);
        assert!(hint.contains("graphics driver"));
        assert!(hint.contains("NVIDIA"));
    }

    #[test]
    fn diagnose_detects_vulkan_failure() {
        let stderr = "vulkan: failed to initialize\nERROR: no Vulkan devices found";
        let hint = diagnose_proton_failure(stderr, 1);
        assert!(hint.contains("Vulkan"));
        assert!(hint.contains("PROTON_USE_WINED3D"));
    }

    #[test]
    fn diagnose_empty_for_unknown() {
        let hint = diagnose_proton_failure("some random output", 1);
        assert!(hint.is_empty());
    }
}
