# dropin

**Download it. Double-click it. Done.**

A cross-distro, cross-desktop installer for all the weird files the internet
hands you — `.AppImage`, `.deb`, `.tar.gz`, `.flatpakref`, `.sh` installers —
with one shared install ledger so everything it installs, it can cleanly remove.

Fully offline. No AI, no accounts, no telemetry, no network calls beyond the
package managers you already have.

## How it works

| You downloaded | dropin does (user scope, no root) |
|---|---|
| `.AppImage` | Moves to `~/Applications`, makes executable, extracts icon + menu entry |
| `.deb` | Extracts payload to `~/.local/opt/<pkg>`, adopts bundled `.desktop` files (paths rewritten), symlinks `usr/bin/*` → `~/.local/bin` |
| `.tar.gz` / `.tar.xz` / `.zip` | Sniffs app-vs-data; app archives → `~/.local/opt/<app>` + menu entry + `~/.local/bin` links. Data archives are refused with an explanation |
| `.flatpakref` / `.flatpak` | `flatpak install --user` |
| `.sh` / `.run` / `.bin` | Asks for explicit confirmation, then runs the vendor installer (tracked as external) |

`dropin info <file>` always shows the dry-run plan first. Every managed path
is recorded in SQLite (`~/.local/share/dropin/ledger.db`); `dropin remove`
reverses it.

## Usage

```
dropin info <file>        # dry run: what would happen
dropin install <file>     # install (shows plan, asks for scripts)
dropin install --yes <file> --keep-source
dropin install --system <file.deb>   # real system install via apt/dnf/pacman (polkit)
dropin list               # everything dropin installed
dropin remove <name>      # clean removal, reversed from the ledger
dropin doctor             # check helper tools
dropin integrate          # double-click handling + “Install as application”
                          # right-click entries for Dolphin/Nemo/Nautilus/Thunar
dropin unintegrate        # remove all desktop integration
dropin update --all       # check everything for updates (on demand only)
dropin update <name>      # check one app
```

`dropin-gui <file>` is the graphical installer dialog (also what double-click
opens after `dropin integrate`).

## Build

```
cargo build --release
```

## Status

M1 (core + CLI) done. Next: M2 GUI double-click dialog, M3 file-manager
integration, M4 update awareness. See `DESIGN.md`.
