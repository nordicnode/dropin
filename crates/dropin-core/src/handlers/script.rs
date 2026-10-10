//! Installer scripts (.sh/.run/.bin): never run silently. They execute
//! with the user's full privileges, so this handler requires explicit
//! confirmation and records the install as external (we can't track what
//! a third-party script does to the system).

use super::{sha256_of, InstallOptions, InstallOutcome, InstallPlan};
use crate::{detect, Error, Result};
use std::io::{self, Write};
use std::path::Path;

pub fn plan(src: &Path) -> Result<InstallPlan> {
    let d = detect::detect(src)?;
    Ok(InstallPlan {
        app_name: d.name_hint.clone(),
        version: None,
        steps: vec![
            "You confirm, then the vendor's installer script runs with YOUR privileges".into(),
            "dropin records it as an external install (file list unknown)".into(),
        ],
        needs_root: false,
        needs_confirmation: true,
    })
}

fn confirm(app: &str, src: &Path, opts: &InstallOptions) -> Result<()> {
    if opts.yes {
        return Ok(());
    }
    let sha = sha256_of(src).unwrap_or_else(|_| "unavailable".into());
    let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    println!("This is a vendor installer script. It will run with your user privileges");
    println!("and can do anything you can do. Only continue if you trust the source.");
    println!();
    println!("  app:    {app}");
    println!("  file:   {}", src.display());
    println!("  size:   {size} bytes");
    println!("  sha256: {sha}");
    print!("\nRun this installer? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        Ok(())
    } else {
        Err(Error::Refused("installer declined by user".into()))
    }
}

pub fn install(src: &Path, opts: &InstallOptions) -> Result<InstallOutcome> {
    let d = detect::detect(src)?;
    confirm(&d.name_hint, src, opts)?;
    let s = src.to_str().ok_or_else(|| Error::Unsupported("bad path".into()))?;
    // .sh files run under sh; .run/.bin are executed directly (makeself etc).
    // Vendor scripts are sandboxed with bubblewrap: they get a throwaway
    // HOME, read-only system, and only the dropin install dirs writable.
    // Fail closed: refuse to run unsandboxed.
    let status = run_sandboxed(src, s)?;
    if !status.success() {
        return Err(Error::Command("installer exited with an error".into()));
    }
    Ok(InstallOutcome {
        name: crate::slugify(&d.name_hint),
        version: None,
        install_root: None,
        paths: vec![],
        external: true,
        update_info: None,
    })
}

/// Run a vendor installer under bubblewrap. The sandbox gets:
/// - no network, no new privileges, dies with the parent
/// - read-only /usr, /etc, and HOME...
/// - ...except ~/.local and ~/Applications, which are writable (the only
///   places a rootless install should touch)
/// - sensitive dirs (~/.ssh, ~/.gnupg) hidden via tmpfs
///
/// Refuses to run if bwrap is missing (fail closed).
fn run_sandboxed(src: &Path, src_str: &str) -> Result<std::process::ExitStatus> {
    if !crate::have("bwrap") {
        return Err(Error::Refused(
            "bwrap (bubblewrap) is required to run installer scripts safely, but it isn't installed.\n\
             Install it for your distro (e.g. `apt install bubblewrap`) and try again."
                .into(),
        ));
    }
    let home = crate::home_dir();
    let home_s = home.to_str().unwrap_or("/tmp");
    let local = home.join(".local");
    let apps = home.join("Applications");
    let _ = std::fs::create_dir_all(&local);
    let _ = std::fs::create_dir_all(&apps);

    let is_sh = src.extension().map(|e| e == "sh").unwrap_or(false);

    // Ensure executable for direct runs.
    if !is_sh {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(src)?.permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(src, perms)?;
    }

    let mut cmd = std::process::Command::new("bwrap");
    cmd.args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--ro-bind", "/usr", "/usr",
        "--ro-bind", "/etc", "/etc",
        "--tmpfs", "/tmp",
        "--proc", "/proc",
        "--dev", "/dev",
        // HOME read-only, then punch writable holes for install dirs.
        "--ro-bind", home_s, home_s,
        "--bind", local.to_str().unwrap_or("/tmp"), local.to_str().unwrap_or("/tmp"),
        "--bind", apps.to_str().unwrap_or("/tmp"), apps.to_str().unwrap_or("/tmp"),
        // Hide secrets even from reading.
        "--tmpfs", &format!("{home_s}/.ssh"),
        "--tmpfs", &format!("{home_s}/.gnupg"),
    ]);
    if is_sh {
        cmd.arg("sh").arg(src_str);
    } else {
        cmd.arg(src_str);
    }
    cmd.status()
        .map_err(|e| Error::Command(format!("running sandboxed installer: {e}")))
}
