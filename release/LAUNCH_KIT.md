# dropin v0.1.0 — launch kit (DRAFT, uncommitted)

Release is live: https://github.com/nordicnode/dropin (tag v0.1.0, 12MB tarball).

## Timing

**Don't post Friday night.** Launch posts sink on weekends. Aim **Monday Oct 12, ~8–9 AM PT** for both. "New release" news value holds for about a week — after that, skip it.

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

v0.1.0 adds a GUI-first manager (per-app update / rollback / remove, health
checks, GitHub release tracking), offline checksum verification, native zsync
delta updates with pre-update snapshots and one-click rollback, and a doctor
that finds orphaned installs and broken menu entries.

https://github.com/nordicnode/dropin — MIT. Happy to answer questions about
the zsync implementation or the pure-Rust .rpm reader.
```

URL: https://github.com/nordicnode/dropin

## Post 2 — r/linux

Title:

```
dropin v0.1.0 – universal Linux installer: double-click any AppImage/.deb/.rpm/tarball, rootless, with a real uninstall ledger
```

Body:

```
I'm the author — dropin v0.1.0 is out.

It's the "double-click anything" installer for Linux: magic-byte detection
across .AppImage/.deb/.rpm/.tar.gz/.zip/.flatpakref/.sh, installs rootless
into ~/Applications + ~/.local/opt, and tracks every path in a SQLite ledger
so uninstalls are actually clean (`dropin remove`, or `--purge` for leftovers).

v0.1.0 is GUI-first (manager window with per-app update/rollback/remove/track,
check-for-updates, doctor), plus offline checksum verification, native zsync
delta updates with snapshots + one-click rollback, and GitHub release tracking
for AppImages without zsync channels.

Install:
  tar -xzf dropin-v0.1.0-x86_64-linux.tar.gz && cd dropin-v0.1.0 && ./install.sh

Repo + release: https://github.com/nordicnode/dropin (MIT, Linux x86_64)
```

## Checklist

- [ ] Monday ~8–9 AM PT: submit Show HN (link post)
- [ ] Same window: post to r/linux (follow subreddit self-promo norms — engage in comments)
- [ ] First hour: reply to every comment/question; that's what drives ranking
- [ ] Optional: r/AppImage and r/opensource later in the week if the first two land

## Notes

- Screenshots already exist in the repo (assets/screenshot-*.png) — HN link posts
  pull the repo's social preview; no extra media needed.
- If Show HN gets zero traction in 2 hours, don't repost — normal for niche tools.
