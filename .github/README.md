# Spotifast (Berkay's build)

This is Berkay's own build of [Spotifast](https://github.com/crmne/spotifast), a
native Spotify client written in Rust. It started as a fork of
**[crmne/spotifast](https://github.com/crmne/spotifast)**, and nearly all of
the app is crmne's work. Playback comes from
**[librespot](https://github.com/librespot-org/librespot)**, the open-source
Spotify Connect library. Please credit and support the upstream projects; for
what Spotifast is, how to use it and how to build it from source, read
[upstream's README](https://github.com/crmne/spotifast#readme) and
[its website](https://spotifast.rocks/). Playback needs Spotify Premium.

This repository ships Windows x64 builds only. The other platforms are
upstream's.

## What this build adds

On top of crmne's Spotifast (see the [release notes](https://github.com/Berkay2002/spotifast/releases)
for what each version changed):

- **An AI DJ** that picks and plays music through your own Claude Code, and
  asks before it removes or renames anything. See [docs/_reference/dj.md](../docs/_reference/dj.md).
- **Transitions between songs**: Off (gapless, the default), Crossfade or
  Automix in Settings, and Mix playlists play their own transitions.
- **Upcoming songs load ahead** into the audio cache (`src/prefetch.rs`).
- **Mirror and LED visualizers** in the player bar, next to Spectrum and
  Waveform.
- **The sidebar takes the page's cover colour.**
- **Choose what the Library sidebar shows**: its sections, a sort control,
  and By You.
- **Shelves scroll with arrows** instead of a scroll bar.
- **An opt-in frame profiler** (the `profiling` Cargo feature).
- **The artwork disk cache stays under 512 MiB.**
- **Playlists still list while Spotify's Web API turns requests away.**
- **A patched librespot**, [Berkay2002/librespot](https://github.com/Berkay2002/librespot)
  branch `spotifast`, that the features above rely on.

## Install on Windows

1. Open [Releases](https://github.com/Berkay2002/spotifast/releases/latest).
2. Download the `.msi` (or the `-setup.exe`) and run it, or download the `.zip`,
   extract it and run `spotifast.exe`.

Each release also has `checksums.txt` and `checksums.txt.sig`. SmartScreen may
warn about an unknown publisher on first run.

## Updates

The app updates itself from [this repository's releases](https://github.com/Berkay2002/spotifast/releases)
(`src/updates.rs`). It installs an update only if the release's `checksums.txt`
is signed with this build's own update key (`assets/update-public-key.hex`),
which is different from crmne's key.

An existing install of crmne's Spotifast follows crmne's releases and
signature, so it will never offer this build as an update. Install this build
manually from Releases once; after that it updates itself.

## Staying in sync with upstream

`main` is the app. crmne's newest `main` commit that passed crmne's CI is
merged into `main` as a merge commit, with this build's features kept where
the two conflict, by the [`spotifast-sync` skill](../.claude/skills/spotifast-sync/SKILL.md)
(run by a scheduled routine or by hand). The librespot fork is moved up to
crmne's librespot pin the same way. This build numbers its own versions. A push
to `main` whose `Cargo.toml` version has no release yet is built, signed and
published by [`release-app.yml`](workflows/release-app.yml). Nothing is sent
back to crmne.

## License

MIT, the same as upstream. See [LICENSE](../LICENSE): copyright (c) 2026 Carmine
Paolino.
