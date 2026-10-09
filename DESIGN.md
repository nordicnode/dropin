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
    dropin-cli/       # `dropin install|remove|list|info|doctor`
    dropin-gui/       # M2: double-click dialog (iced)
```

### Format handling

| Download | Default action (user scope) |
|---|---|
| `.AppImage` | Move to `~/Applications`, `chmod +x`, extract icon + name from embedded `.desktop`, write menu entry |
| `.deb` | Extract `data.tar.*` to `~/.local/opt/<pkg>`, adopt bundled `.desktop` (paths rewritten), symlink `usr/bin/*` → `~/.local/bin`. Optional: system install via `apt` + polkit |
| `.rpm` | Same strategy via `rpm2cpio` (M1.1) |
| `.flatpakref` / `.flatpak` | `flatpak install --user` |
| `.tar.gz` / `.tar.xz` / `.tar.bz2` / `.zip` | Sniff contents: app-looking (ELF executables, `.desktop`, Electron `app.asar`) → extract to `~/.local/opt/<app>`, adopt or generate `.desktop`, symlink binaries. Data-looking → refuse, suggest the archive manager |
| `.sh` / `.run` / `.bin` | Confirm, then execute (tracked as "external": we record what we *can't* track) |
| `.exe` / `.msi` | Hand to Wine if present, else explain |

Detection is by magic bytes first, extension second — never extension alone.

### The ledger

SQLite at `~/.local/share/dropin/ledger.db`:

- `installs(id, name, format, version, source_path, source_sha256, install_root, external, created_at)`
- `files(install_id, path)` — every path created/moved, for reversal

`remove` deletes recorded paths (files, symlinks, `.desktop` entries, icons),
then drops the rows. Anything installed with root (`external` system installs)
gets removed through the same package manager that installed it.

### Security model

- Scripts require explicit confirmation; the GUI shows publisher, size, sha256.
- `.sig`/`.asc` sidecar files are verified when present (warn, don't block, when absent).
- System installs only via an explicit `--system` flag → polkit prompt.
- Path traversal in archives is rejected (no `../`, no absolute paths on extract).

## Milestones

- **M1 (done 2026-10-09):** `dropin-core` + `dropin-cli`. Detection, ledger, AppImage /
  tarball / deb / flatpakref / script handlers, `install|remove|list|info|doctor`.
- **M1.1 (done 2026-10-09):** pure-Rust `.rpm` handler (RPM header + cpio reader,
  no rpm2cpio needed), rootless like `.deb`.
- **M2 (in progress):** `dropin-gui` double-click dialog (egui): app name, version,
  what-will-happen, Install/Cancel, progress, trust checkbox for scripts.
  `dropin integrate` registers MIME handlers (AppImage/deb/rpm/flatpakref/scripts;
  archives deliberately stay with the archive manager).
- **M3 (done 2026-10-09):** File-manager integration — `dropin integrate` now also
  installs "Install as application" context entries for Dolphin (service menu),
  Nemo (.nemo_action), Nautilus (Python extension), and Thunar (uca.xml merge
  with backup + idempotent reinstall); `unintegrate` removes everything.
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
