# dropin — design doc (working name)

**Tagline:** Download it. Double-click it. Done.

## Problem

On Linux, the same "Download" button hands you a `.tar.gz`, a `.deb` built for
the wrong distro, an `.AppImage` that won't run until you `chmod +x`, or a
`.sh` that opens in a text editor. Every format has its own ritual, its own
tool, and — worst of all — no shared record of what you installed, so
uninstalling is archaeology.

## Prior art (and the gap)

| Project | Scope | Limitation |
|---|---|---|
| `exeify` (new, ~Sept 2026) | Double-click install for many formats | KDE Plasma + Arch only; shell script; no uninstall ledger |
| AppImageLauncher | AppImage integration | AppImages only; Qt/C++ |
| Gear Lever / AppImage Integrator | AppImage management | AppImages only |
| AM / AppMan | Portable app installs | AppImage-centric, CLI-first |
| PkgManager | GUI over apt/dnf/pacman/flatpak | System packages only, not stray downloads |

**The gap:** a cross-desktop, cross-distro, multi-format installer with a real
install ledger — so everything it installs, it can cleanly remove. Fully
offline. No AI, no accounts, no network calls except the package managers you
already have.

## Principles

1. **User scope by default.** Installs land in `~/Applications`,
   `~/.local/opt/<app>`, `~/.local/bin`, `~/.local/share/applications` —
   never root unless the user explicitly chooses a system install.
2. **Explicit consent for executables.** `.sh`/`.run`/`.bin` never run without
   a clear "Run installer?" confirmation. Checksums shown when available.
3. **Dry run first.** `dropin info <file>` shows exactly what *would* happen
   before anything is touched.
4. **Ledger everything.** Every file the installer creates or moves is recorded
   in SQLite. `dropin remove` reverses it. If we can't track it, we say so.
5. **Never phones home.** No telemetry, no update checks without being asked.

## Architecture

```
dropin/
  crates/
    dropin-core/      # detection, handlers, ledger, .desktop generation
    dropin-cli/       # `dropin install|remove|list|info|doctor|...`
```

The GUI was removed in v0.2.1; dropin is CLI-only.

### Format handling

| Download | Default action (user scope) |
|---|---|
| `.AppImage` | Move to `~/Applications`, `chmod +x`, extract icon + name from embedded `.desktop`, write menu entry |
| `.deb` | Extract `data.tar.*` to `~/.local/opt/<pkg>`, adopt bundled `.desktop` (paths rewritten), symlink `usr/bin/*` → `~/.local/bin`. Optional: system install via `apt` + polkit |
| `.rpm` | Same strategy through a pure-Rust RPM/cpio reader (no `rpm2cpio` needed) |
| `.flatpakref` / `.flatpak` | `flatpak install --user` |
| `.tar.gz` / `.tar.xz` / `.tar.bz2` / `.zip` | Sniff contents: app-looking (ELF executables, Windows `.exe`, `.desktop`, Electron `app.asar`) → extract to `~/.local/opt/<app>`, adopt or generate `.desktop`, symlink binaries. Windows games get a Wine/Proton prefix. Data-looking → refuse, suggest the archive manager |
| `.sh` / `.run` / `.bin` | Audit, confirm, then run sandboxed under bubblewrap with a hardlink snapshot for undo |
| `.exe` / `.msi` | Wine for apps, GE-Proton via umu-launcher for games; isolated prefix per app |

Detection is by magic bytes first, extension second — never extension alone.

### The ledger

SQLite at `~/.local/share/dropin/ledger.db`:

- `installs(id, name, format, version, source_path, source_sha256, install_root, external, update_info, verified, github_repo, signing_key, env_vars, created_at)`
- `files(install_id, path)` — every path created/moved, for reversal

`remove` deletes recorded paths (files, symlinks, `.desktop` entries, icons),
then drops the rows. System installs (`--system`) are removed through the same
package manager that installed them; flatpaks through `flatpak uninstall`;
adopted games keep the user's files and only dropin's launchers are removed.

### Security model

- Scripts require explicit confirmation; the CLI shows the audit findings, size, and sha256.
- `.sig`/`.asc` sidecar files are checked when present; the issuer key is pinned (TOFU) and updates refuse a changed, missing, or unreadable signature once a key is pinned.
- System installs only via an explicit `--system` flag → polkit/sudo prompt.
- Path traversal in archives is rejected (no `../`, no absolute paths on extract).

## Milestones

- **M1 (done 2026-10-09):** `dropin-core` + `dropin-cli`. Detection, ledger, AppImage /
  tarball / deb / flatpakref / script handlers, `install|remove|list|info|doctor`.
- **M1.1 (done 2026-10-09):** pure-Rust `.rpm` handler (RPM header + cpio reader,
  no rpm2cpio needed), rootless like `.deb`.
- **M2 (removed in v0.2.1):** the `dropin-gui` double-click dialog and the
  `dropin integrate` file-manager hooks were removed when the project went
  CLI-only. The CLI is the supported interface.
- **M3 (removed in v0.2.1):** file-manager integration (`integrate` /
  `unintegrate`) was part of the GUI-era surface and is no longer shipped.
- **M4 (done 2026-10-09):** Update awareness — `dropin update [name|--all]`, on demand
  only, never in the background. The ledger stores each AppImage's embedded
  `X-AppImage-UpdateInformation` channel at install (schema migrated); update
  runs `appimageupdatetool` where a channel exists, delegates flatpaks to
  `flatpak update --user`, and honestly reports "check upstream manually" for
  everything else (deb/rpm via system package manager, tarballs re-download).
- **Phase 1 — Trust (done 2026-10-09):** integrity verification + safe metadata.
  `dropin install` checks companion checksum files (`foo.sha256`, `SHA256SUMS`,
  …) in the download directory — fully offline; a mismatch **refuses** the
  install with expected/actual hashes (Cemu-proof), a match records
  `verified` in the ledger and shows a ✓ badge in `info`/`list`/GUI (the GUI
  disables Install on mismatch). AppImage `.desktop`/icon extraction now reads
  the squashfs payload at its magic-byte offset with pure-Rust `backhand` —
  `--appimage-extract` (which executes the image) is only a fallback for
  type-1 images.
- **Phase 2 — Intelligence (done 2026-10-09):** `dropin doctor` now runs health
  checks beyond helper tools: orphaned ledger entries (files deleted by hand),
  duplicate installs (same menu name across formats), `.desktop` shadowing (a
  user entry hiding a system one of the same basename — the dead-launcher
  trap), and cross-source installs (same app via dpkg/rpm/flatpak → duplicate
  menu entries). `dropin remove --purge` moves leftover `~/.config`,
  `~/.local/share`, `~/.cache` dirs to the freedesktop trash — never deletes;
  only name-matching dirs created at/after install time qualify, and the purge
  is skipped entirely if the app is still provided elsewhere.

## Open questions

- Project name: `dropin` is provisional. Alternatives: `justinstall`, `anypkg`.
- deb-on-non-debian: rootless extraction works everywhere; system install only
  where apt/dnf/pacman exist. Foreign-arch debs get a warning, not a block.
