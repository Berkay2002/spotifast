---
name: spotifast-sync
description: Merge crmne/spotifast's latest release into this app (Berkay2002/spotifast, Berkay's own Spotifast) - bring the librespot fork up to crmne's pin, merge the release tag into main resolving every conflict so the app's own features win, bump the version, verify, and push so the Release workflow publishes it. Use this whenever the user, or a scheduled routine, wants the app updated from crmne, synced, or on crmne's new version, asks whether crmne has released something new, or asks why the app is behind - even if they only say "sync spotifast" or "update the app".
---

# Sync from crmne

This repository is Berkay's own app, started as a fork of crmne/spotifast.
`main` is the app. crmne's releases are an update source: each one is merged
into `main` once, as a merge commit, so a conflict is resolved once and Git
remembers it. Nothing is sent to crmne.

After the merge is pushed, `.github/workflows/release-app.yml` builds, signs
and publishes the Windows release for the version in `Cargo.toml`.

This may run unattended (a scheduled Claude routine, no one to ask) or with
the user present. Where the steps say "ask", see "When a decision is needed".

## Setup

- crmne as a remote: `git remote add crmne https://github.com/crmne/spotifast.git`
  if `git remote -v` lacks it. Fetch release tags into a private ref, never
  into `refs/tags`, where crmne's and the app's version tags would clash:
  `git fetch --no-tags crmne "refs/tags/$TAG:refs/crmne/tags/$TAG"`.
- The librespot fork, `Berkay2002/librespot` branch `spotifast`: a clone
  next to this repository (in a routine it is attached and already cloned;
  locally use `../_scratch/librespot` or clone it there). It needs a remote
  `crmne` = https://github.com/crmne/librespot.git.
- Work on `main` in a checkout with no uncommitted changes. Locally, if the
  user's checkout has work in progress, use a worktree
  (`git worktree add ../_scratch/sync main`) and check
  `git rev-parse --show-toplevel` before merging: a merge in the wrong
  directory lands on whatever the user has checked out.
- On Windows Git Bash, prefix `git show <ref>:<path>` with
  `MSYS_NO_PATHCONV=1` for that command only.

## Steps

1. **Is there anything new?** `gh api repos/crmne/spotifast/releases/latest --jq .tag_name`
   (REST works everywhere; `gh release` and `gh issue` use GraphQL, which a
   cloud session can't reach). Fetch the tag as above. If
   `git merge-base --is-ancestor refs/crmne/tags/$TAG main`, it is merged
   already: report "up to date with crmne $TAG" and stop.

2. **librespot.** crmne pins every librespot crate to one commit:
   `git show refs/crmne/tags/$TAG:Cargo.toml | grep 'github.com/crmne/librespot'`.
   In the librespot clone, `git fetch crmne` and check
   `git merge-base --is-ancestor <crmne pin> origin/spotifast`. If it is not
   there yet, merge it into `spotifast` (`git merge --no-ff <crmne pin>`, not a
   rebase: `main`'s history already points at the current commits), resolve,
   `cargo check --workspace`, and push `spotifast`. Note the new tip SHA.

3. **Merge the release into `main`.**
   `git merge --no-ff -m "Merge crmne $TAG" refs/crmne/tags/$TAG`.
   Resolve each conflict by kind:
   - **`Cargo.toml` version line**: keep the app's own version. The app
     numbers its own releases; crmne's number means nothing here.
   - **librespot lines in `Cargo.toml`**: every librespot crate from
     `https://github.com/Berkay2002/librespot` at the `spotifast` tip from
     step 2, all on the same rev. Keep crmne's comment about its patches and
     the app's paragraph about what `spotifast` adds. Do this even when the
     merge didn't conflict there, if step 2 moved the tip.
   - **`Cargo.lock`**: take crmne's (`git checkout --theirs Cargo.lock`) and
     let `cargo check` rewrite the app's entries. Hand-edited lock hashes are
     wrong in a way only the build notices.
   - **Translations** (`assets/i18n/*.po`, `spotifast.pot`): these conflict
     because the `#: src/...:LINE` location comments moved, not because the
     strings clash. Keep both sides' entries; the location comment belongs to
     crmne's side. The `.po` files are data the build validates, so
     `cargo test --locked --test localization` confirms the result.
   - **Docs**: crmne's text, with the app's sentences put back where they
     describe one of the app's features.
   - **Code**: read each side's history for the hunk
     (`git log -p --merge -- <file>`) until you can say what each change is
     for. crmne's change comes in; the app's feature is re-applied on top of
     the new code so it behaves as it did. When both change the same
     behaviour in contradictory ways, the app's behaviour wins, written
     against crmne's new structure. When crmne's change only adds to or
     improves the same area (as crmne's scrolling header wash did next to the
     app's sidebar tint), take both.
   - Ask when crmne's change makes one of the app's features redundant (it
     now does the same thing differently), or a resolution changes what the
     user sees and no test or `demo` screenshot can confirm it.

   Write the merge commit message as: `Merge crmne $TAG`, a blank line, then
   one line per non-mechanical resolution (what crmne changed, how the app's
   feature was kept).

4. **Release it.** Bump `Cargo.toml`'s version: the patch number for a
   crmne patch release, the minor number for a crmne minor release. Write
   `packaging/release-notes/v<new version>.md`: one sentence that it brings in
   crmne $TAG with a link to
   `https://github.com/crmne/spotifast/releases/tag/$TAG`, then crmne's notes
   for that tag (`packaging/release-notes/$TAG.md` from the merge) under a
   `## From crmne $TAG` heading, then any app-side change worth a user's
   notice. Commit "Release <new version>".

5. **Verify.** Linux needs `libasound2-dev libpulse-dev libxkbcommon-dev
   libwayland-dev libgl1-mesa-dev cmake libclang-dev`. Then:
   `cargo fmt --all --check`,
   `cargo clippy --locked --all-targets -- -D warnings`,
   `cargo test --locked --all-targets`,
   `python3 .github/scripts/check-dependency-sources.py`.
   A cold build takes well over ten minutes: run it in the background and
   wait for it. A failure caused by the merge is fixed in a follow-up commit
   before pushing; a failure that also happens on crmne's tag alone is
   crmne's, noted in the report and left alone.

6. **Push** `main` (a normal push; it only adds commits). The push starts
   the Release workflow. Find the run with
   `gh api "repos/Berkay2002/spotifast/actions/runs?branch=main&per_page=1"`
   and check it until it completes (about 25 minutes). If it fails, read the
   failed job's log (`gh api repos/Berkay2002/spotifast/actions/runs/<id>/jobs`)
   and fix forward.

7. **Report**: the new version and crmne tag, the release URL, each
   non-mechanical resolution in one line, and anything left undone.

## When a decision is needed

With the user present, ask, then continue.

Unattended, don't guess on what the user will see. Push the merge as it
stands, with the questionable part resolved the way that keeps the app's
current behaviour, to a branch `claude/sync-$TAG` instead of `main`, and open
an issue through REST:
`gh api repos/Berkay2002/spotifast/issues -f title="Sync crmne $TAG: decision needed" -f body="..."`
naming the branch, the choice, and what each option would look like. The
next run sees the open issue (`gh api "repos/Berkay2002/spotifast/issues?state=open"`)
and stops instead of redoing the work.

## Boundaries

- Never push to crmne's repositories, and never rebase or force-push `main`
  or librespot's `spotifast`: other checkouts build on them.
- Release tags are created by the Release workflow, not pushed by hand.
