# Changelog

All notable changes to dropin are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project uses
[Semantic Versioning](https://semver.org/).

## [0.2.2] — 2026-11-12

### Fixed

- **RPM installs now work on real packages.** The main RPM header is not
  padded to 8 bytes, so the payload offset was 7 bytes late; the cpio name
  padding also ignored the 110-byte header. Both are fixed, and the test
  fixture now matches real RPMs.
- **Script sandbox works with a normal home.** `$HOME` is set to the bound
  home, `/tmp` is mounted before the home/src binds, and missing `~/.ssh` /
  `~/.gnupg` no longer abort the sandbox.
- **Failed script installs are reverted automatically** instead of leaving
  partial files the ledger can't reach. Empty parent dirs are cleaned up too.
- **`dropin launch` finds the right executable.** It reads the tracked
  desktop entry (not just `dropin-<slug>.desktop`), resolves bare `Exec=`
  names via `~/.local/bin` and `PATH`, and prefers the binary matching the
  app when a package ships several.
- **Bundled `.desktop` files with `Exec=myapp`** now resolve to `bin/myapp`
  or `usr/bin/myapp` instead of a non-existent `<root>/myapp`.
- **Adopted Windows games** now remove their launchers and prefix while
  keeping the user's game files.
- **`--system` deb/rpm installs** are removed through the package manager,
  and are refused on distros that can't install that package format.
- **`set-env` / `unset-env`** now fail for apps that aren't installed.
- **`dropin list | head`** no longer panics on a broken pipe.
- **`dropin info`** no longer leaks a temp extraction dir per run.
- **GitHub updates** prefer the asset matching the app name and avoid
  `obsolete`/`debug`/`symbols` assets.
- **Signature continuity** refuses an update that drops or breaks a
  previously pinned signature; malformed signatures are reported, not
  ignored.
- **Flatpak removal** uses the real app id stored at install time.
- **Zip detection** works by magic even when the file is renamed.
- **`doctor`** checks `bwrap`/`umu-run` and portable Wine, and broken-menu
  detection covers every tracked desktop entry, not just AppImages.

### Hardening

- All HTTP requests now have connect/response/body timeouts, so a stalled
  server can't hang `dropin update` forever.
- Wine installers run with a wall-clock timeout (default 2 hours, override
  with `DROPIN_INSTALL_TIMEOUT`); a hung installer is killed and its
  half-created prefix is removed.
- The script sandbox probes bubblewrap before running, so a system with
  unprivileged user namespaces disabled gets a clear explanation instead of
  a cryptic mid-install failure.
- Added tests for Wine executable selection, runner choice, the umu GAMEID
  disk cache, HTTP body caps, and the process timeout helper.

### Changed

- External `curl`, `tar`, `unxz`, and `unzstd` calls were replaced with the
  in-process HTTP client and pure-Rust archive decoders.
- umu GAMEID lookups are cached on disk, so a title only hits the network
  once.
- `dropin info` shows the static script audit.
- CI now also checks `cargo fmt --check`.
- Release binaries are built by `.github/workflows/release.yml` and attached
  to GitHub Releases instead of being committed to git.

## [0.2.1] — 2026-10-11

### Changed

- CLI-only release. The GUI and file-manager integration were removed.
- Release tarball contains only the `dropin` binary, README, and installer.

## [0.2.0] — 2026-10-09

### Added

- AppImage generation layout with atomic `active` symlink and rollback.
- zsync delta updates, GitHub release tracking, and signature pinning.
- Sandboxed installer scripts with audit, logging, and undo.
- `doctor`, `remove --purge`, `export`/`import`, `adopt`, `set-env`.
- Pure-Rust `.deb` and `.rpm` readers, Windows `.exe`/`.msi` via Wine/Proton.
