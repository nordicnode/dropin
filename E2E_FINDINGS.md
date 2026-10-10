# E2E Testing Findings — Real Games (2026-10-10)

Tested with **real games**, not mocks:
1. **BrogueCE Linux** v1.15.1 (911KB .tar.gz) — native Linux roguelike, ELF binary
   - Source: https://github.com/tmewett/BrogueCE/releases/download/v1.15.1/BrogueCE-1.15.1-linux-x86_64.tar.gz
2. **Spelunky Classic** 1.1 (9.6MB .zip) — Windows freeware game, contains `Spelunky.exe` + `config.exe`
   - Source: http://www.derekyu.com/games/spelunky_1_1.zip
3. **BrogueCE Windows** v1.15.1 (1.7MB .zip) — Windows build, contains `brogue.exe`
   - Source: https://github.com/tmewett/BrogueCE/releases/download/v1.15.1/BrogueCE-1.15.1-windows-x86_64.zip

Test environment: `DROPIN_HOME=/tmp/dropin-e2e-test` (isolated). No Wine/umu installed (intentional, for error-path testing).

---

## Critical Bugs

### 1. Cannot install .zip files containing Windows games (SEVERE)
**Repro:** `dropin install spelunky_1_1.zip` or `dropin install brogue-windows.zip`
**Result:** `refused: ... looks like data, not an application (no executables or .desktop files found)`
**Root cause:** `handlers/tarball.rs::sniff_dir()` only checks `is_elf_executable()` — it never looks for Windows `.exe` files (MZ magic). The tarball handler (used for .zip) is ELF-only.
**Impact:** Most Windows games are distributed as .zips. This breaks the entire Windows-game-from-zip flow. Bare `.exe` files work (Wine handler), but zips don't.
**Note:** The error message is also wrong — it says "looks like data" when it's actually a valid Windows application dropin can't detect.

### 2. Adopted games leave orphaned .desktop files on remove (SEVERE)
**Repro:**
1. Adopt a game (via `adopt::adopt()` — no CLI exists, used core directly)
2. `dropin remove <name>`
3. Check `~/.local/share/applications/` — `dropin-<name>.desktop` still exists

**Root cause:** In `install.rs::remove()`, when `inst.external == true`, the code removes the ledger entry and returns early without deleting any tracked files. But adopt creates files (the .desktop, bin symlink) that should be cleaned up. The game files themselves should be preserved (correct), but dropin's own files are orphaned.
**Impact:** Menu entries pile up for every adopted-then-removed game. User has to manually delete them.

### 3. Adopt silently skips bin symlink on name collision (MODERATE)
**Repro:** Adopt a game whose binary name matches an existing `~/.local/bin/` symlink.
**Result:** No symlink created, no warning to user. The adopted game has no CLI launcher.
**Root cause:** `adopt.rs` checks `if !link.exists()` and skips silently. The install path (`link_bin`) instead removes and replaces.
**Inconsistency:** Install overwrites, adopt skips silently. Neither warns the user.
**Suggestion:** Adopt should use the slug name for the symlink (e.g., `brogue-adopted`) to avoid collisions, or warn.

---

## UX Issues & Friction

### 4. `install` says "Link executables (3)" but creates 1 symlink
**Repro:** `dropin install brogue-linux.tar.gz`
**Output:** `Link executables into ~/.local/bin (3)` but only 1 symlink appears in `~/.local/bin/`.
**Root cause:** The count is from the sniff phase (found 3 executables: wrapper script, `make-link-for-desktop.sh`, ELF binary), but only the main binary gets linked. The number is misleading.
**Fix:** Say "Link main executable" or show the actual count linked.

### 5. `list` shows vague format: "brogue — compressed archive"
The format string "compressed archive" is not useful. It should say the detected type (e.g., "tar.gz", "zip") or better yet, the app name. Compare: `brogue-adopted — adopted [external]` is much clearer.

### 6. .desktop for terminal games has `Terminal=false`
**Repro:** Install BrogueCE (a curses/terminal roguelike).
**Result:** `.desktop` has `Terminal=false`. Clicking the menu entry launches it without a terminal — it will fail silently or flash and disappear.
**Fix:** Detect terminal/curses apps (e.g., check if binary links against libncurses, or if it's a shell script wrapper) and set `Terminal=true`. Or prompt the user.

### 7. .desktop for games has `Categories=Utility;`
BrogueCE got `Categories=Utility;` instead of `Game;`. The category detection isn't working for tarball installs. Games should be categorized as games.

### 8. `launch` reports success even when binary crashes immediately
**Repro:** `dropin launch brogue` (missing libSDL2_image on system)
**Result:** `Launched 'brogue'.` — but the binary immediately exits with "error while loading shared libraries."
**Root cause:** `launch()` spawns detached with stdio=null and returns success if spawn succeeds. It never checks if the process actually runs.
**Suggestion:** At minimum, check `ldd` for missing libraries before launching ELF binaries and warn. Or spawn, wait 500ms, check if still alive, and report early exit.

### 9. Adopt remove message is confusing
**Output:** `'brogue-adopted' was installed externally (adopted); removed its ledger entry, but files installed by the vendor script / system package manager remain — use that tool to fully uninstall`
**Problems:**
- "files installed by the vendor script / system package manager" — wrong for adopt. The files are the user's own game folder. There is no "vendor script."
- "use that tool to fully uninstall" — what tool? The user just pointed at a folder.
- Doesn't mention that the .desktop was (supposed to be) removed.
**Fix:** For adopted games: "Removed 'X' from dropin. Your game files in /path/to/game were left in place."

### 10. No `adopt` CLI subcommand (GUI-only feature)
The adopt feature (new in this release) has no CLI equivalent. `dropin adopt --help` → "unrecognized subcommand." Users who prefer CLI (or scripts) cannot adopt games. This is friction, especially since the task description says "I only want the GUI" but the CLI is "retained for scripting" — adopt should be scriptable too.

### 11. Bare .exe install works well (POSITIVE)
`dropin install brogue.exe` (no Wine installed) gives:
> `refused: Wine isn't installed, so Windows apps can't run yet. Run 'dropin setup-wine' or use the GUI to set it up automatically.`
This is a clear, actionable error. Good UX. The Windows zip path should give a similarly helpful message (once bug #1 is fixed).

### 12. Duplicate install handled gracefully (POSITIVE)
Second `dropin install` of the same game: `refused: ~/.local/opt/brogue already exists — remove it first with 'dropin remove brogue'`. Clear and actionable.

### 13. Corrupted file handled gracefully (POSITIVE)
`dropin install corrupt.zip`: `Couldn't read the zip file. It may be corrupted or not actually a zip archive. Details: invalid Zip archive: Could not find EOCD`. No panic, helpful message.

### 14. Remove/launch non-existent handled gracefully (POSITIVE)
Both give clear `not installed: <name>` errors. No panic.

---

## Adopt Flow Findings (via core library directly)

### 15. Adopt correctly identifies main executable
For BrogueCE (which has `brogue` wrapper script, `bin/brogue` ELF, and `make-link-for-desktop.sh`), adopt correctly chose the `brogue` wrapper script as the main executable. The heuristic works.

### 16. Adopt scan is noisy (MINOR)
`scan_dir` found 3 executables including `make-link-for-desktop.sh` (a helper script, not a game launcher). In the GUI, the user would see 3 options in the dropdown. The chooser should deprioritize scripts with names like "make-link", "uninstall", "setup", etc. (It already avoids "uninstall" in `choose_main_exe`, but `make-link-for-desktop.sh` slips through.)

### 17. Adopt tracks files in place correctly (POSITIVE)
The adopted game's files in `/tmp/adopt-game/` were not moved or modified. The ledger correctly recorded them as external. `dropin remove` did not delete the user's game files. This is the correct behavior for adopt-in-place.

---

## Summary by Severity

| # | Issue | Severity |
|---|-------|----------|
| 1 | .zip with Windows games rejected (sniff_dir is ELF-only) | **Critical** |
| 2 | Adopted games leave orphaned .desktop on remove | **Critical** |
| 3 | Adopt silently skips bin symlink on collision | Moderate |
| 4 | "Link executables (3)" creates 1 symlink (misleading count) | Moderate |
| 6 | Terminal games get `Terminal=false` in .desktop | Moderate |
| 7 | Games get `Categories=Utility;` instead of `Game;` | Moderate |
| 8 | `launch` reports success when binary crashes immediately | Moderate |
| 9 | Adopt remove message is confusing/wrong | Minor |
| 5 | `list` shows vague "compressed archive" format | Minor |
| 10 | No `adopt` CLI subcommand (GUI-only) | Minor (friction) |
| 16 | Adopt executable picker is noisy | Minor |

**Positive:** Bare .exe Wine error, duplicate install refusal, corrupted file handling, remove/launch non-existent errors, adopt-in-place file preservation, and adopt executable chooser all work well.

---

## Test Artifacts
- Downloads: `/tmp/dropin-e2e-test/downloads/` (spelunky_1_1.zip, brogue-linux.tar.gz, brogue-windows.zip)
- Test HOME: `/tmp/dropin-e2e-test/` (isolated, safe to delete)
- Adopt test game: `/tmp/adopt-game/BrogueCE-linux/`
