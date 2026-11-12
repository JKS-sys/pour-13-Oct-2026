# Pour — features

Kept up to date after every release, so the lists can be copied onto ipconfig.co.network as they are.
Last updated: 12 Nov 2026 · Pour 0.1.14

Pour has **no paid plan**. Everything below is free for everyone; there is nothing to unlock.

## Free (everyone)

### Install and manage
- Installs Homebrew formulae and casks, with every dependency, in parallel
- `install`, `uninstall`, `reinstall` (downloads a fresh copy), `upgrade`, `update`, `outdated`
- `search`, `info`, `list`, `deps` (with a coloured tree), `uses`, `leaves`
- `why <name>` — shows what pulled a package in
- `size` — disk space per package, largest first, with a bar for each
- `pin` / `unpin`, `link` / `unlink`, `cleanup`
- `bundle` — install from a Brewfile, or write one
- `migrate` — move everything Homebrew installed over to Pour
- `services` — run a package in the background (launchd on macOS, systemd on Linux)
- `doctor` — checks the installation and names any command that leads nowhere, with the fix
- `self-update` — Pour updates itself, checksum verified
- Upgrading a library also upgrades the packages built on it

### Where packages come from
- All of Homebrew: 8,600+ formulae and 7,800+ casks, synced every few hours
- pour-apps: every app made at ipconfig.co.network (`pour install --cask peek`)
- pour-tools: command-line tools with checked downloads for systems Homebrew has no build for
- pour-desktop: desktop apps for Windows and Linux under the same names as Homebrew's casks
- pour-windows: Windows software synced from the Scoop buckets
- `register` — list your own app or tool; checked automatically, no waiting for approval
- Extra registries with `tap`

### Systems
- macOS (Apple silicon and Intel), Linux (x86_64, arm64, i686), Windows (x86_64, arm64), FreeBSD

### Colour, motion and sound
- Every kind of thing has its own colour: names, versions, paths, commands, flags, numbers; brackets by depth; no pink
- Framed log boxes for caveats, warnings, hints and long errors, with numbered, highlighted lines
- Short animations with a sound for install, update, upgrade, remove, cleanup, search, info, bundle, link, migrate, doctor and errors
- Plain text in pipes and scripts; `POUR_ANIMATIONS=0` and `POUR_SOUND=0` turn motion and sound off
- `--json` output for scripts

### Website
- ipconfig.co.network/pour: search every package, app pages, docs, one-line install
- Pages open from an edge copy (about 0.2 s)

## Paid

None. Pour is free.

## Changes

- 12 Nov 2026 — website pages served from an edge copy; this file added
- 7 Nov 2026 (0.1.14) — log boxes, `why`, `size`, error animation and sound; keys from the dashboard vault
- 5 Nov 2026 (0.1.13) — npm missing after installing node fixed; `doctor` names broken commands
- 4 Nov 2026 (0.1.12) — last pink removed; four new colours; radar, bubbles, rain and bridge animations
- 1 Nov 2026 (0.1.11) — upgrading a library upgrades what is built on it; `reinstall` downloads afresh
