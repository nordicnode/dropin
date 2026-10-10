# dropin v0.2.0 — launch kit (DRAFT)

Release is live: https://github.com/nordicnode/dropin (tag v0.2.0, 15MB tarball: dropin 11MB + dropin-gui 28MB, install.sh, README).

## Timing

**Don't post Friday night.** Launch posts sink on weekends. Aim **Monday Oct 12, ~8–9 AM PT** for both. "New release" news value holds for about a week — after that, skip it.

## Decide before Monday

Two features landed on main AFTER the v0.2.0 tag (commits a5f3c9c and 832c78f):

- **"Add existing game…"** — adopt pre-installed games Lutris-style: pick a folder, dropin scans for executables, registers it in the library with a .desktop entry and uninstall tracking, without moving anything.
- **Proton launch diagnostics** — failed game launches capture output to proton-last-run.log and the error dialog gives actionable advice (driver issues, Vulkan failures) instead of a bare exit code.

Options: (a) roll **v0.2.1** before Monday and announce that instead — the "add existing game" story is the strongest hook; or (b) announce v0.2.0 as-is and hold the rest. Your call.

## Post 1 — Hacker News (Show HN)

Title:

```
Show HN: Dropin – double-click installer for every Linux package format
```

Body:

```
I got tired of the ritual: download some tarball, guess what's inside it,
hand-write a .desktop file, and then never be able to uninstall it cleanly.
So I built dropin.

It detects the format by magic bytes — AppImage, .deb, .rpm, tarballs, zips,
flatpakrefs, installer scripts — installs rootless into ~/Applications +
~/.local/opt, and records every path in a SQLite ledger so `dropin remove`
reverses the install exactly. Fully offline, no root, no telemetry.

v0.2.0 is GUI-first: a manager window (per-app update / rollback / remove /
track, check-for-updates, doctor), pure-Rust archive extraction so installs
never shell out to unzip/tar/ar, an extraction cache so big archives aren't
unpacked three times, a dark-theme redesign, and plain-language error dialogs
with a copy button. Proton/GE-Proton support with zero-friction Wine setup
means Windows games install and run from the same flow. 89 tests passing.

https://github.com/nordicnode/dropin — MIT. Happy to answer questions about
the pure-Rust .rpm reader or the ledger design.
```

URL: https://github.com/nordicnode/dropin

## Post 2 — r/linux

Title:

```
dropin v0.2.0 – universal Linux installer: double-click any AppImage/.deb/.rpm/tarball, rootless, with a real uninstall ledger
```

Body:

```
I'm the author — dropin v0.2.0 is out.

It's the "double-click anything" installer for Linux: magic-byte detection
across .AppImage/.deb/.rpm/.tar.gz/.zip/.flatpakref/.sh, installs rootless
into ~/Applications + ~/.local/opt, and tracks every path in a SQLite ledger
so uninstalls are actually clean (`dropin remove`, or `--purge` for leftovers).

v0.2.0 brings pure-Rust archive handling (no external tools), an extraction
cache, a dark theme redesign, better error messages with a copy button,
Proton/GE-Proton game support, and drag-and-drop install.

Install:
  tar -xzf dropin-v0.2.0-x86_64-linux.tar.gz && cd dropin-v0.2.0 && ./install.sh

Repo + release: https://github.com/nordicnode/dropin (MIT, Linux x86_64)
```

## Checklist

- [ ] Decide: announce v0.2.0, or roll v0.2.1 first (adds "Add existing game")
- [ ] Eyeball assets/screenshot-*.png — the v0.2.0 dark theme changed the UI, regenerate any that show the old look
- [ ] Monday ~8–9 AM PT: submit Show HN (link post)
- [ ] Same window: post to r/linux (follow subreddit self-promo norms — engage in comments)
- [ ] First hour: reply to every comment/question; that's what drives ranking
- [ ] Optional: r/AppImage and r/opensource later in the week if the first two land

## Notes

- If Show HN gets zero traction in 2 hours, don't repost — normal for niche tools.
