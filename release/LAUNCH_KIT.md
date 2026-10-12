# dropin v0.2.1 — launch kit

Status (verified 2026-10-11): **v0.2.1 is rolled and live.** Tag v0.2.1 pushed,
CLI-only tarball published at
https://github.com/nordicnode/dropin/releases/tag/v0.2.1 (contains only
README.md, the dropin binary, and the CLI-only rootless install.sh — no GUI
binary), release notes written from the CLI-only draft, and the three stale
GUI screenshots moved to `archive/`. The v0.2.0 tarball still ships
dropin-gui; announce v0.2.1 only.

## Timing

**Don't post Friday night.** Launch posts sink on weekends. Aim **Monday Oct 12,
~8–9 AM PT** for both. "New release" news value holds for about a week — after
that, skip it.

## Before Monday — all done 2026-10-11 (verified)

- [x] Tag **v0.2.1** from main and rebuild the release tarball (dropin binary
      only; dropin-gui is gone). The v0.2.0 tarball still ships the GUI.
- [x] Write the GitHub release notes from the CLI-only draft in
      `release/v0.2.1-notes.md`.
- [x] Move the stale GUI screenshots out of `assets/` (screenshot-manager.png,
      screenshot-install-dialog.png, screenshot-done.png). The README only
      uses screenshot-terminal-install.png now.

Remaining for Monday: only Mikey's own-hardware `dropin adopt` test (his
call whether to hold `adopt` out of the announcement if untested).

## Post 1 — Hacker News (Show HN)

Title:

```
Show HN: Dropin – point it at a Linux installer file and it's installed
```

Body:

```
I got tired of the ritual: download some tarball, guess what's inside it,
hand-write a .desktop file, and then never be able to uninstall it cleanly.
So I built dropin. Point it at a file and it's installed.

It detects the format by magic bytes — AppImage, .deb, .rpm, tarballs, zips,
flatpakrefs, installer scripts, even Windows .exe/.msi. Installs rootless
into ~/Applications + ~/.local/opt, and records every path in a SQLite ledger
so `dropin remove` reverses the install exactly. Fully offline, no root,
no telemetry.

v0.2.1 is CLI-only (I ripped the GUI out): `dropin info <file>` dry-runs any
file before it touches anything, installer scripts stay sandboxed and need
your explicit confirmation, and `dropin remove` is the whole undo story.
New: `dropin adopt <dir>` registers an already-installed game folder
Lutris-style — scans for executables, writes the .desktop entry, tracks it for
clean removal, moves nothing. Proton/GE-Proton support with automatic setup
(`dropin setup-wine` / `dropin setup-proton`) and launch diagnostics that tell
you what's actually wrong (driver issues, Vulkan failures) instead of a bare
exit code.

95 tests passing. https://github.com/nordicnode/dropin — MIT. Happy to answer
questions about the pure-Rust .rpm reader or the ledger design.
```

URL: https://github.com/nordicnode/dropin

## Post 2 — r/linux

Title:

```
dropin v0.2.1 – universal Linux installer, now CLI-only: one command for AppImage/.deb/.rpm/tarballs, with a real uninstall ledger
```

Body:

```
I'm the author — dropin v0.2.1 is out.

It's a universal installer for Linux, CLI-only: point it at a file and it's
installed. Magic-byte detection across .AppImage/.deb/.rpm/.tar.gz/.zip/
.flatpakref/.sh (plus Windows .exe/.msi via Wine/Proton). Installs rootless
into ~/Applications + ~/.local/opt, and a SQLite ledger tracks every path so
`dropin remove` is actually clean (`--purge` moves leftovers to the trash).

New since v0.2.0: the GUI is gone (CLI-only going forward), `dropin adopt`
adds an already-installed game folder Lutris-style without moving anything,
and Proton launch failures now produce actionable diagnostics instead of a
bare exit code. Plus a full E2E pass against real games: Windows zip fixes,
adopt cleanup, and 7 UX fixes.

Install:
  tar -xzf dropin-v0.2.1-x86_64-linux.tar.gz && cd dropin-v0.2.1 && ./install.sh

Repo + release: https://github.com/nordicnode/dropin (MIT, Linux x86_64)
```

## Checklist Monday

- [ ] Monday ~8–9 AM PT: submit Show HN (link post)
- [ ] Same window: post to r/linux (follow subreddit self-promo norms — engage in comments)
- [ ] First hour: reply to every comment/question; that's what drives ranking
- [ ] Optional: r/AppImage and r/opensource later in the week if the first two land

## Notes

- If Show HN gets zero traction in 2 hours, don't repost — normal for niche tools.
