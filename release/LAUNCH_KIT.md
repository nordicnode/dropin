# dropin v0.2.1 — launch kit

Status: v0.2.1 needs to be rolled from main before Monday. The v0.2.0 tag and
tarball predate the GUI removal and the adopt + E2E fixes (and the v0.2.0
tarball still ships dropin-gui), so the announced version should be v0.2.1,
built from current main.

## Timing

**Don't post Friday night.** Launch posts sink on weekends. Aim **Monday Oct 12,
~8–9 AM PT** for both. "New release" news value holds for about a week — after
that, skip it.

## Before Monday (needs your go-ahead)

- [ ] Tag **v0.2.1** from main and rebuild the release tarball (dropin binary
      only; dropin-gui is gone). The v0.2.0 tarball still ships the GUI.
- [ ] Write the GitHub release notes from the CLI-only draft in
      `release/v0.2.1-notes.md`.
- [ ] Move the stale GUI screenshots out of `assets/` (screenshot-manager.png,
      screenshot-install-dialog.png, screenshot-done.png). The README only
      uses screenshot-terminal-install.png now.

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
