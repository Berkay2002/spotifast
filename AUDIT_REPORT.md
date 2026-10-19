# Audit report: audit/2026-10-18

Date: 2026-10-18. Branch: `audit/2026-10-18`, 45 fix commits on top of `main`, not pushed.

## Summary

**Project.** Spotifast is a native Spotify desktop client in Rust 2024 (toolchain pinned to 1.98.0). It uses egui/eframe 0.36 and winit from crmne's forks, librespot 0.8 patched to `Berkay2002/librespot@44c8444` for playback and Connect, tokio and reqwest (rustls) for the network, and crmne's fastframe crates for files, tray, updates, single instance, audio, now playing, i18n and power. `Berkay2002/spotifast` is Berkay's own app, started as a fork of crmne's. Its Windows x64 release comes from `.github/workflows/release-app.yml` with Ed25519-signed checksums. The code is about 132k lines of Rust; `src/app.rs` alone is 27.8k lines and `src/backend.rs` 9k.

**Scope.** The whole repository: app state and queue, pages and UI, sign-in and credentials, the Web API and backend, the player and audio chain, settings and files, LAN receivers, IPC and self-update, the AI DJ, and the build, CI, packaging and docs.

**Method.** The code was split into 9 slices, each reviewed on its own:

1. Sign-in, grants, credential storage, proxy and engine connection
2. Web API client, session reads, backend request dispatch and caches
3. Local network receivers, single-instance IPC, links, self-update, entrypoint and platform shims
4. AI DJ: child process, MCP tool server, tool trust boundary and approvals
5. Player engine, audio sink, prefetch, transitions, EQ/limiter and visualiser taps
6. Settings, state files, disk caches, skins and custom themes
7. App state machine: playback intent, optimistic UI, queue and library mutations
8. Page loading, API response handling, action application and the UI layer
9. Build script, dependencies, CI/release workflows, policy scripts, packaging and docs accuracy

Each finding was verified independently before any fix. Each fix was made with a regression test where the code allowed one, then reviewed again before it was kept. A final pass re-ran every check that works on this machine.

| Count | |
|---|---|
| Raw findings | 59 |
| Distinct after merging duplicates | 57 |
| Confirmed in verification | 53 |
| Dropped in verification | 4 |
| Fixed and kept | 44 findings in 45 commits (2 high, 19 medium, 23 low) |
| Attempted but not kept | 2 (1 dropped by review, 1 skipped as already fixed) |
| Intentionally left alone | 7 |

**Models.** Opus@high throughout (finding, verification, fixing and review).

## Baseline and final checks

All Rust checks here ran with `--no-default-features` (see the second table for why).

| Check | Baseline | After |
|---|---|---|
| `cargo fmt --all --check` | pass | pass |
| `cargo clippy --locked --all-targets --no-default-features -- -D warnings` | pass, 0 warnings | pass, 0 warnings |
| `cargo clippy ... --no-default-features --features demo -- -D warnings` | pass | pass |
| `cargo test --locked --all-targets --no-default-features` | 1361 passed, 1 ignored | 1396 passed, 1 ignored (+35 tests) |
| `cargo test ... --no-default-features --features demo` | 1393 passed, 1 ignored | 1428 passed, 1 ignored |
| `cargo test ... --features demo --doc` | pass (0 doc tests) | pass (0 doc tests) |
| `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --no-default-features` | pass | pass |
| `python .github/scripts/check-dependency-sources.py` | pass | pass |
| `python .github/scripts/check-review-evidence.py` | pass | pass |
| `python .github/scripts/test-policy-checks.py` | 16 tests OK | 18 tests OK |
| `python .github/scripts/check-new-settings.py --base main` | pass (no diff) | pass on the real 43-commit diff (before the two skin commits, which add no settings), without `--approved` |
| `python packaging/test-release-names.py` and `packaging/flatpak/test-metainfo.py` | 4 + 4 OK | 4 + 4 OK |

The one ignored test is `credentials::tests::native_store_round_trip`. No check regressed and no fix was reverted in the final pass.

**Checks that could not run here:**

| Check | Why |
|---|---|
| Default features (`milkdrop`): clippy, test, doc, and every `--all-features` variant | `projectm-sys` needs vcpkg GLEW (`VCPKG_INSTALLATION_ROOT`). With a local vcpkg some fixers got default-feature clippy clean, but the test binary still fails to link (`glewInit` unresolved). CI covers this. No fix touched MilkDrop code. |
| `python packaging/test-launchers.py` | Linux-only: it sources the Arch PKGBUILD files through bash and calls Linux tools. 8 errors on Windows before and after. No fix touched `packaging/`. |
| `bash .github/scripts/update-translations.sh --check` | At baseline it exited 1 silently on Windows (fixed by F58). It now runs, but exits 1 because the `.pot` source line references were already stale. |
| `native_store_round_trip` (ignored test) | Writes dummy grants into the real Windows Credential Manager. CI runs it on Windows and macOS. |
| `bundle exec jekyll build` | No Ruby here. |
| `nix build .#default` | No Nix on Windows. |
| Linux and macOS builds | Only Windows was compiled. Fixes with `cfg` code (F23, F47's Linux-only MPRIS test, F22's unix test branch) were reasoned about on the other targets, not compiled. |

## Fixed

Ordered by severity. "Behaviour" marks a public behaviour change; details follow the table.

| Id | Severity | Title | Location | Commit | Tests | Behaviour |
|---|---|---|---|---|---|---|
| F41 | high | A Previous that only restarts the song left the queue-start flag set, so the next queued song was never consumed | src/app.rs:9769 | 79cb2d1 | `previous_that_restarts_the_song_still_lets_the_queue_advance` (fails without fix) | yes |
| F52 | high | spotifast-sync picked the newest green CI run on any branch named main, including fork pull requests | .claude/skills/spotifast-sync/SKILL.md:43 | 3e47368 | Query run live against the API; ancestry guard checked by hand | no |
| F1 | medium | A 429 or 408 token refresh deleted the saved Web API grant | src/auth.rs:322 | f7e7307 | `only_an_oauth_error_rejects_the_grant` | yes |
| F2 | medium | connect_engine dropped an in-flight Web sign-in's cancel sender, so the listener busy-looped and could not be cancelled | src/backend.rs:3401 | 6812181 | `a_dropped_cancel_sender_ends_the_redirect_listener`, `engine_reconnect_keeps_a_web_sign_in_cancellable` (fails without fix) | no |
| F3 | medium | Declining consent on Spotify's page was ignored, so setup hung for 10 minutes | src/auth.rs:248 | d2adca4 | `declining_on_spotify_ends_the_redirect_listener`, extended `request_line_parsing` | yes |
| F4 | medium | A refreshed (possibly rotated) grant restored from storage was not saved until verification succeeded | src/api/client.rs:181 | 8d0e34b | `a_restored_web_grant_saves_its_refreshes_before_verification` (fails without fix) | yes |
| F6 | medium | A rejected refresh during 401 recovery was swallowed, so the sign-in prompt never appeared | src/api/client.rs:103 | 399864c | `a_401_whose_refresh_is_refused_reports_the_expired_sign_in` (fails without fix) | yes |
| F16 | medium | Forget and CancelSchedule ran without approval in the DJ's own (autopilot and scheduled) turns | src/app/dj.rs:273 | 9227def | `the_djs_own_turns_can_neither_change_notes_nor_schedules` | yes |
| F18 | medium | Dj::shutdown kept chat_id and notes_sent, so the next conversation overwrote the saved chat and lost the notes | src/dj/session.rs:508 | a3b2591 | `after_shutdown_the_next_turn_is_a_new_chat_and_gets_the_notes` (fails without fix) | yes |
| F19 | medium | DJ notes, schedules and chats were written with blocking std::fs on the UI thread | src/dj/agent.rs:27 | 12fb595 | `saves_are_handed_to_the_writer_and_never_wait_for_the_disk_here`; two tests updated | no |
| F25 | medium | Any EQ change during playback reset every biquad's history, so the output clicked | src/eq.rs:472 | b36ee9a | `changing_the_settings_mid_tone_does_not_click` (fails without fix, step 0.29) | yes |
| F28 | medium | Limiter look-ahead under-reduced short transients, which hit the hard clamp | src/limiter.rs:93 | bcd8348 | `a_short_burst_is_caught_without_clipping` (fails without fix) | yes |
| F30 | medium | An unreadable settings.json was reset to defaults and then overwritten | src/settings.rs:704 | 19bd7f0 | `an_unreadable_settings_file_is_kept_beside_it` | yes |
| F31 | medium | Recents kept the previous account's Spotify history after switching accounts | src/app.rs:2534 | ef95c00 | `recents_do_not_follow_an_account_switch` (fails without fix) | yes |
| F43 | medium | A failed queue refresh replaced the shown queue with an error, dropping optimistic rows | src/app.rs:6109 | 4b3b575 | `a_failed_queue_refresh_keeps_the_shown_queue` (fails without fix) | yes |
| F44 | medium | A failed remote shuffle command left the shuffle button stuck | src/app.rs:8709 | ebc64d0 | `a_refused_remote_shuffle_is_undone_by_the_next_poll` (fails without fix) | yes |
| F45 | medium | Queue answers during a batch write used up the stale-answer allowance, so the next read skipped every staleness check | src/app.rs:6098 | 4b0ab39 | `a_long_queue_write_does_not_let_a_stale_answer_undo_a_skip` (fails without fix) | no |
| F47 | medium | MPRIS OpenUri sent web and spotify:// links to PlayContext unconverted | src/app.rs:3911 | cc766e1 | Extended the Linux-only MPRIS test (not run: Linux only) | yes |
| F51 | medium | Release could be dispatched from any branch, and the signing key had no protected environment | .github/workflows/release-app.yml:151 | 2d704f6 | YAML parsed; workflow not run on GitHub | no (CI only) |
| F32 | medium | A crafted .wsz could exhaust memory: skin bitmaps decoded with the image crate's 512 MiB limit and no size cap | src/skin/mod.rs:47 | 9bd025f | `an_oversized_sheet_is_skipped_for_the_built_in_one` | no |
| F55 | medium | The privacy page left out the AI DJ's data flow | docs/_reference/privacy.md:38 | bfc48f5 | Docs; claims checked against the code | no |
| F8 | low | Untrusted LRC timestamps could overflow u32 arithmetic | src/lyrics.rs:566 | bc21dad | `an_oversized_lrc_stamp_is_skipped` | no |
| F9 | low | The lyrics cache used blocking, non-atomic std::fs on the async runtime | src/lyrics.rs:587 | 69d1f18 | `a_cached_answer_is_replaced_whole_in_a_folder_made_for_it` | no |
| F11 | low | A LAN-supplied Sonos deviceID went unvalidated into authenticated spclient paths | src/sonos.rs:228 | 0105807 | Extended `discovery_uses_receiver_names_and_ids_and_omits_dead_advertisements` | yes |
| F12 | low | getInfo and addUser read unbounded response bodies from LAN devices | src/zeroconf.rs:226 | 5c79274 | `an_oversized_receiver_reply_is_refused` | no |
| F13 | low | The missing-OpenGL help link used open::that, which launches PowerShell on Windows | src/window.rs:85 | bbfd34c | None (modal dialog plus shell launch) | no |
| F14 | low | CLI control output printed Spotify metadata raw, so terminal escapes passed through | src/entrypoint.rs:318 | 4716aa9 | `control_output_does_not_pass_escape_sequences_to_the_terminal` | yes |
| F17 | low | Due DJ schedules were used up while the DJ could not run, then fired late or under another account | src/app/dj.rs:1647 | 4dcbb8c | `a_task_due_while_the_dj_cannot_run_is_not_marked_as_run` (fails without fix) | yes |
| F21 | low | ensure_child held the state mutex while starting Claude Code, so Stop, sign-out and quit froze the UI | src/dj/claude.rs:558 | 0784952 | `a_stop_or_shutdown_does_not_wait_for_a_child_being_started` (fails without fix) | no |
| F22 | low | Claude CLI discovery accepted relative PATH entries | src/dj/exe.rs:208 | 0591553 | Extended `candidates_come_in_the_order_override_path_home_npm` | no |
| F23 | low | On Windows the MCP token file had no owner-only ACL | src/dj/private.rs:17 | 72937bb | `only_the_owner_can_enter_the_folder_or_read_the_file` (Windows) | no |
| F26 | low | Background prefetch of upcoming tracks was missing from the network docs | docs/_reference/how-it-connects.md:185 | 7f2332f | Docs; figures checked against src/prefetch.rs | no |
| F29 | low | Mix recipes were never fetched when turned on in the same playlist, and a failed fetch was not retried | src/transition/mod.rs:253 | 126a062 | `recipes_are_fetched_once_turned_on_and_again_after_a_failed_fetch` | yes |
| F33 | low | The skin zip reader had no total inflate budget, so duplicate entries could peg a core for hours | src/skin/mod.rs:118 | e9869b7 | `a_name_repeated_across_the_directory_is_inflated_once` (fails without fix), `a_skin_that_inflates_past_the_limit_is_refused`; follow-up f2f2a78: `a_name_whose_last_copy_cannot_be_read_is_not_tried_again`, `an_unpacked_skin_larger_than_the_limit_is_refused` (both fail without it) | yes (f2f2a78) |
| F34 | low | `--device-name`, documented as session-only, was saved to settings.json | src/entrypoint.rs:485 | e35d569 | `a_command_line_device_name_is_never_saved` | yes |
| F35 | low | Clear artwork cache deleted the cover folder synchronously on the UI thread | src/app.rs:10614 | f95fd01 | Two existing tests moved onto an async helper; one now checks the file is deleted | no |
| F37 | low | Docs gave the wrong MilkDrop preset default and a wrong softened-preview claim | docs/_reference/settings-and-files.md:312 | f267b6e | Docs | no |
| F38 | low | privacy.md put the session and play history in the config directory | docs/_reference/privacy.md:23 | d682d5e | Docs | no |
| F39 | low | check-new-settings.py ignored settings structs not named ...Settings (LibrarySidebar) | .github/scripts/check-new-settings.py:24 | aa8acbc | Two new policy tests (fail on the old checker) | no |
| F40 | low | The first Winamp playlist draw scanned every system font on the UI thread | src/system_fonts.rs:30 | 264ea8f | `the_face_is_ready_once_the_search_ends`, `lines_drawn_while_the_face_was_looked_for_are_redrawn_once_it_is_found` | yes |
| F48 | low | A late Episode link answer navigated after the user had moved on | src/app.rs:7345 | 0f87fd2 | `a_late_episode_answer_does_not_undo_later_navigation` | yes |
| F50 | low | The DJ crate repainted at 30 fps whatever the power budget | src/ui/dj_crate.rs:297 | a95a71e | `the_dj_disc_turns_at_half_rate_out_of_focus` | yes |
| F56 | low | Settings & Files left out every DJ setting and the per-account DJ files | docs/_reference/settings-and-files.md:11 | 7ef98d9 | Docs | no |
| F58 | low | update-translations.sh could not find fastframe-i18n on Windows, and its bug URL sent translators to crmne | .github/scripts/update-translations.sh:7 | 47ba17d | Run on Windows; pipeline fed Windows, Unix and missing paths | no |

**Public behaviour changes worth knowing about:**

- **F41**: a queued song now leaves Playing next once it plays, even after a restart-only Previous. Edge case: a requested start that takes more than 6 s to show up is treated as a natural advance and may consume the head of the manual queue if it is the same song.
- **F1**: a 429, 408, or a 4xx with no OAuth error code no longer deletes the grant. A permanent 4xx with an unrecognised body is now retried (capped at 60 s) instead of signing out. Code-exchange errors say "token request failed" instead of "rejected".
- **F3**: Cancel on Spotify's consent page ends the flow right away ("Spotify authorization was declined", or the Failed state with Try again). The browser failure page wording changed slightly.
- **F4**: a grant restored at launch saves refreshed tokens before /me verifies it. A fresh browser sign-in still waits for verification.
- **F6**: a 401 followed by a refused refresh now asks for a new sign-in at once. An unreachable token endpoint during 401 recovery is now a network error.
- **F16**: autopilot and scheduled DJ turns can no longer forget notes or cancel schedules (matches what the docs already promised).
- **F17**: a DJ schedule due while the DJ cannot run waits on disk and fires only within the 2-hour catch-up window. Sign-out drops waiting scheduled prompts.
- **F18**: after sign-out or switching the DJ off and on, the next DJ chat is new and gets the notes.
- **F25**: EQ changes are smooth. Preamp changes and EQ on/off now take one audio packet (tens of ms) instead of being instant.
- **F28**: short loud transients are limited smoothly; release can start up to 8 ms later.
- **F29**: Mix recipes load after turning them on mid-playlist; failed fetches retry at most once a minute.
- **F30**: an unreadable settings, session or history file is kept as `<name>.corrupt-<time>` and a warning is logged.
- **F31**: after any Connected, Recents' Spotify history is cleared and fetched again; local plays stay.
- **F34**: `--device-name` is really session-only now.
- **F40**: a playlist drawn before the font search ends shows Inter briefly, then the found face.
- **F43, F44, F48**: a failed queue refresh keeps the shown queue; a refused remote shuffle reverts on the next poll; a late episode answer no longer navigates.
- **F47** (Linux): `playerctl open` with open.spotify.com or spotify:// links now plays; track and episode links play on their own.
- **F11, F14**: receivers with malformed device ids are no longer listed; CLI control output prints control characters as U+FFFD.
- **F33 follow-up (f2f2a78)**: each skin file name is tried once. A duplicate whose last copy cannot be read no longer falls back to an earlier copy. An unpacked skin folder over 32 MiB in total is refused.
- **F50**: the DJ disc runs at 30 fps in focus (it really ran at about 60 before), 15 under Saver, none in Background.

**Partial fixes and open edges noted by review:**

- F19: the one-time DJ file reads in `set_account` still run on the UI thread.
- F30: the strict enums (ThemeChoice, VisMode, LibraryShelf/LibrarySort, ProxyMode, transition::Mode) were not made tolerant, so one unknown value still resets the session to defaults (the file is kept, though). A file that can be neither read nor renamed is still replaced (marked `ponytail:`).
- F44: a poll that lands before the error arrives can still leave the button stuck (about one round trip in every 4 s). A ShufflePlay whose set_shuffle fails comes back as RemoteAction::Play and is not covered.
- F51: the code change only restricts the branch. Moving the key into the environment is a manual step (see next steps).
- F58: the `.pot`/`.po` files were not regenerated; the new header arrives with the next translation update.

## After the workflow

The workflow's fix agents ran unattended; afterwards the orchestrating session:

- **Closed two gaps in F33 (f2f2a78)** that an automated security review flagged on e9869b7: the .wsz reader marked a name as read only after a clean read, so repeated entries that each failed after inflating were each inflated again; and the unpacked-folder reader capped each file but not the total. Reviewed independently (pass).
- **Re-landed F32 (9bd025f)** from `refs/audit-dropped/F32` with LF endings, a 29-line diff. Reviewed independently against image 0.25.10 (pass): legitimate sheets up to 2048x2048, 16-bit and palette files included, decode; a 2049 px side or more than 16 MiB is refused. The worst case is about 240 MiB for 15 sheets, against about 7.5 GiB before.
- **Added the missing blank line** after the subject of two commit messages (264ea8f, 47ba17d); later short shas changed accordingly. The pre-rewrite tip is kept at `refs/audit-backup/pre-reword`.
- Re-ran fmt, both clippy variants, both test suites and rustdoc: all pass.

## PR review follow-ups

Codex's review of PR #2 found three gaps in audit fixes. Each was fixed, independently reviewed (pass) and covered by a test that fails without it:

- **1e897ab** (F3 follow-up): only `access_denied` reports the sign-in as declined; any other OAuth error on the redirect ends it as a failed authorization.
- **18cb97e** (F17/F18 follow-up): any change to a different account, not only Sign out, shuts the DJ down and drops waiting scheduled tasks. A same-account reconnect keeps it.
- **712f91e** (F19 follow-up): each account's DJ notes, schedules and chats stay in memory across account switches, so switching back before a queued save lands cannot reopen a stale file.

CodeRabbit's review then found six more, each fixed and independently reviewed (pass):

- **4fad6eb** (F39 follow-up): the new-settings check also reads private structs that Settings holds.
- **0697bbe** (F35 follow-up): clearing the artwork cache counts only files it really removed.
- **4e2cf44** (F32 follow-up): skin sheets are capped at 1024 px a side so any permitted 32-bit BMP fits the zip reader's 8 MiB entry limit.
- **0716ac8** (F45 follow-up): a pending queue write no longer polls the queue every 700 ms; its answer refreshes the queue.
- **9387138** (F41 follow-up): a Connect transfer back to this computer slower than 6 s no longer consumes a queued copy of the song.
- The DJ pending-save comment was already fixed by 712f91e.

Not changed, for the maintainer: on Windows the DJ's private folder in TEMP relies on ACLs, which a volume without persistent ACL support (FAT) does not enforce. Refusing the DJ there is a product decision; this predates the audit.

Also left open, minor: a play request held while local playback reconnects uses the same 6 s marker and could count as a queue advance if reconnecting takes longer; and a queue batch whose answer the backend drops after a renewed lease is never cleared, so that target's queue reads wait until the next sign-out (a timeout on `pending_queue_batches` would close it).

Left open by the review, minor: a turn event from the old account can still add a "Stopped." line to the new account's empty DJ panel (no chat is saved); a generation counter in `Dj::shutdown` would close it.

## Attempted but not kept

| Id | Severity | Title | Outcome | Reason |
|---|---|---|---|---|
| F42 | medium | reconcile_pending_queue treated a pending add as landed when its URI appeared anywhere in the fetched queue | Dropped by review, saved at `refs/audit-dropped/F42` | It fixed the named cases but added a regression: a row queued on another client in front of this app's rows stops a landed add from being confirmed, so it shows twice for up to 30 s. It also left "queue a song again after its first copy landed" broken. A better fix subtracts the URI's confirmed manual rows from the prefix counts and confirms adds that sit behind rows queued elsewhere. |
| F20 | low | An unreadable DJ state file was treated as empty and then overwritten | Skipped | Already fixed by F30 (19bd7f0), which moved the DJ's `load` onto `settings::load_json`. |

## Intentionally left alone

| Id | Severity | Title | Location | Why left | Proposed fix |
|---|---|---|---|---|---|
| F5 | medium | An uncapped queue-write Retry-After extends the shared cooldown and stalls every request on that source | src/api/client.rs:597 | Policy trade-off: Spotify's Retry-After applies app-wide, so letting other requests out after 30 s may earn more 429s. Needs a decision on what the session-wide pause should be, then how-it-connects.md should state it. | Extend the shared cooldown with `wait.min(MAX_RETRY_AFTER)` and sleep the rest only inside the queue write while holding `queue_writes`; or keep the full wait and make interactive commands fail fast. Extend the queue rate-limit test. |
| F53 | medium | Signed releases ship without waiting for CI and are built without `--locked` | .github/workflows/release-app.yml:85 | Gating on CI blocks Windows releases on any flaky macOS, ARM, nix or docs job, changes the trigger, and conflicts with the sync skill shipping crmne commits that fail a check. Needs Berkay's call. | Add `--locked` to the build now (safe, one word). Then optionally gate on CI via `workflow_run` with `conclusion == 'success'` and `head_sha`, or a check-runs gate on the required jobs only. |
| F10 | low | ZeroConf addUser hands the reusable credential to any unauthenticated LAN advertiser, and the docs say it cannot | src/zeroconf.rs:348 | Needs a product decision (warnings or confirmation on new receivers). | At minimum correct how-it-connects.md L367-370 to say the receiver is trusted on selection. Better: send a short-lived access token when getInfo advertises token support, and fall back to the blob only otherwise. |
| F36 | low | Local play history and the resume session are shared across accounts and survive sign-out | src/history.rs:46 | Product decision: per-account history or clearing it on sign-out. | Record the account id with history and the resume fields (as CachedRootlist does) and ignore them for another account; or clear them on Sign out and document it in settings-and-files.md and privacy.md. |
| F54 | low | The README shipped in every release points Windows users to crmne's builds and issue tracker | README.md:58 | Public-facing wording and placement are the maintainer's choice, and a README section adds a conflict point to every sync. | A short top section naming Berkay's releases page, update channel and issue tracker, one line each for the AI DJ and transitions, crmne's text kept below as upstream. Optionally point `src/window.rs` DOWNLOAD_PAGE at the releases page (update the test at L241). |
| F57 | low | crmne's triage, board and packaging workflows pass all secrets to external reusable workflows (disabled only in repo settings) | .github/workflows/issue-assessment.yml:39 | Nothing runs today. Deleting the files changes how syncs resolve (modify/delete conflicts), so it is a sync-policy call. | Delete issue-assessment.yml, board.yml and triage-open-backlog.yml and have spotifast-sync keep them deleted, or replace `secrets: inherit` with explicit secrets and pin to a SHA. Moving the signing key into an environment (F51) also removes it from any `inherit`. |
| F59 | low | ci.yml and docs.yml use mutable action tags; the nix job hands a Cachix token to tag-pinned third-party actions | .github/workflows/ci.yml:238 | These are crmne's files; local pins would conflict on every sync, and Dependabot is new config. | Pin ci.yml and docs.yml actions to full SHAs with version comments and keep them current with Dependabot. At minimum pin the three nix-job actions that run before the Cachix secret. |

## Dropped in verification

- **F7** (src/paths.rs:121, raw account id in the playlist cache dir): sign-out purges the whole playlists root, and cache files are keyed by global playlist id, so neither claimed harm can happen.
- **F15** (src/thumbbar.rs:98, English taskbar tooltips): docs/_reference/translating.md already documents the Windows taskbar buttons as English in every language, on purpose.
- **F24** (src/dj/claude.rs:544, random cwd breaks `--resume`): the installed Claude Code (2.1.302) falls back to scanning every project folder for the session id, so resume works.
- **F27** (src/sink.rs:146, `thread::sleep` blocks a tokio worker): the backend runs on its own OS thread through `block_on`, so the sleep parks only that thread, not a runtime worker.

## Recommended next steps

1. **Finish F51 in GitHub settings (manual, high value).** Set the `release-signing` environment's deployment branch policy to `main` only, move `SPOTIFAST_UPDATE_SIGNING_KEY` into that environment and delete the repository-level copy. Until then the workflow change only restricts the branch.
2. **Add `--locked` to the release build** (F53, release-app.yml L85). One word, matches CI. Decide separately whether to gate releases on CI.
3. **Small skin hardening left over** (from the reviews of f2f2a78 and 9bd025f): read unpacked skin files through `take(SKIN_LIMIT + 1)` so a file that grows between `metadata()` and the read cannot pass the cap, and add height and `max_alloc` cases to the F32 test.
4. **Redo F42** with occurrence counting that subtracts already confirmed manual rows and can confirm an add behind rows queued on another client. Keep the three regression tests from `refs/audit-dropped/F42` and add the two probe cases from its review.
5. **Correct the ZeroConf credential claim** in how-it-connects.md L367-370 (F10 minimum), then decide on the token-instead-of-blob option.
6. **Decide F5** (shared cooldown policy) and F36 (per-account history or clear on sign-out), then implement and document.
7. **Close the F44 race**: let the poll handler adopt `shuffle_state` once the hold has expired whatever the previous snapshot said, and cover the ShufflePlay path.
8. **Make the session file enums tolerant** of unknown values (F30 follow-up) so one bad value no longer resets the whole session.
9. **Run CI on this branch** before merging: it covers MilkDrop, Linux and macOS (including F47's MPRIS test, F22's unix branch and F23's cfg split), nix and the docs build, none of which ran here.
10. **Decide F54, F57 and F59** together as a sync policy for crmne's README and workflow files. Regenerate the translation catalogs at the next translation update (F58).
