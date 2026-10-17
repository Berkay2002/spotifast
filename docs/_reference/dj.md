---
title: The AI DJ
description: What the DJ is, what it can do, what it asks you first, and what it keeps.
nav_order: 5
---

The DJ is a chat inside Spotifast that picks and plays music for you. It
does not run in the cloud on Spotifast's account: it drives your own
**Claude Code** (the `claude` program), signed in with your own Claude plan.
Spotifast never sees your Claude login and has no key of its own.

## The page

The top of the page is your queue as a crate of records: the song playing
is the sleeve in front, its disc half out and turning while it plays, and
the songs up next fan out behind it. A change of song slides the disc out
and the next sleeve forward; songs the DJ has just queued are marked with
a green dot for a few seconds. With reduced motion on, the disc does not
turn and nothing slides.

Under it is the chat. For each message you see what the DJ thought (a
collapsed "Thought for 6s", open while it is thinking), what it read and
did (collapsed to "Used 3 tools" once it is done), the songs it queued,
removed or put in a playlist with their covers, and its answer, with
buttons to copy the answer or ask again. The bar at the bottom holds the
message box, the model and how hard it thinks.

## What you need

- Claude Code installed, and `claude login` run once in a terminal.
- Nothing set in your environment that makes Claude Code bill an API key
  (`ANTHROPIC_API_KEY` and similar). The DJ refuses to start while one is
  set, so it never spends anything but your plan.

The DJ page says what is missing and has a **Check again** button.

## What it can do

It has the same abilities you have by hand, and no others.

| Area | Tools |
|---|---|
| Look at your music | `now_playing`, `playback_state`, `queue`, `recent_plays`, `library_summary`, `liked_tracks`, `top_items`, `playlist_tracks`, `album_tracks`, `artist_top_tracks`, `artist_albums`, `track_info`, `devices`, `now` |
| Find music | `search` (songs, artists, albums, playlists) |
| Play | `play`, `queue_tracks`, `pause`, `resume`, `skip`, `previous`, `seek`, `set_volume`, `set_shuffle`, `set_repeat`, `clear_queue`, `remove_from_queue`, `transfer_playback` |
| Change your library | `like_tracks`, `unlike_tracks`, `create_playlist`, `add_to_playlist`, `remove_from_playlist`, `rename_playlist` |
| Keep things | `remember`, `forget`, `memory`, `schedule_task`, `list_schedules`, `cancel_schedule` |

There is no tool that deletes a playlist, unfollows anything or touches
your account. The DJ adds songs to playlists that are yours or collaborative;
it removes songs from and renames only your own.
Spotify's own limits apply: no description removal, no queue clearing on
another device, no tempo or key data.

Results from Spotify (titles, playlist names) are treated as data. The
DJ is told to ignore any instructions inside them.

## What it asks first

With **Ask before changing my library** on (the default), each change that
alters your library waits for a yes or no on the DJ page: likes, new
playlists, adding or removing songs, renaming. A new playlist shows its name,
whether it is private and a few covers, with **Create** and **Cancel**; the
other changes say what they would do with **Approve** and **Cancel**. An
unanswered change is declined after four minutes.

Some changes ask even with that switch off. There is no undo, so removing
songs (from a playlist or from Liked Songs) and renaming always ask. Notes
and scheduled tasks always ask, because they last beyond the chat. And every
change the DJ wants to make in a turn of its own, on autopilot or from a
schedule, asks: nobody was watching, and a web page or a playlist name could
have steered it. Only adding songs, liking and creating playlists run without
asking, and only in a conversation you started, with the switch off.

Playing, queueing and the other playback controls run at once and are
listed in the chat as they happen.

## What it keeps

Everything stays on your computer, next to your settings:

Each account has its own files, so another Spotify account on this computer
does not inherit them: `dj-memory.<account>.json` and the others. Files from
before that (`dj-memory.json`, `dj-schedules.json`, `dj-chats.json`) become
the first account's the next time it opens the DJ. A file that cannot be read
is kept beside it as `<name>.corrupt-<time>` instead of being overwritten.

- `dj-memory.json`: up to 40 short notes ("no country on Sundays"). They go
  to the model at the start of a conversation. You can see and clear them
  by asking the DJ.
- `dj-schedules.json`: up to 10 tasks ("every weekday at 08:30, play
  something gentle"). They run while Spotifast is open.
- `dj-chats.json`: the last 30 chats, so you can reopen one.

## On its own

**Autopilot** (off by default) lets the DJ act without a message from you
while Spotifast is open: it tops up a queue that runs low and reacts when
you skip two songs in a row. It runs at most once a minute and can use a
smaller **Background model** to spare your plan. It never saves notes or
schedules on its own, and its library changes still wait for you.

## The web

**Let the DJ search the web** (on by default) gives it Claude Code's web
search for facts about music. It cannot open pages. Turn it off to keep
the DJ to Spotify only.

## How it is kept safe

- Claude Code runs with none of its own tools (no files, no shell) except
  the optional web search. Spotifast's tools reach it through a server on
  `127.0.0.1` on a random port, protected by a one-time token that never
  appears on a command line.
- No user, project or plugin settings of yours are loaded into it.
- Each tool call is checked (types, counts, URIs by kind) before the app
  acts on it.

## Known gaps

The DJ's text is English only for now; it is not in the translation
catalogues yet. Claude Code is the only provider.

The reasoning and the cards (covers, trays) are drawn from what happened
during the conversation and are not saved: a chat you reopen shows its
messages, answers and the one-line log of what the DJ did. The reasoning
appears only when the Claude Code version in use reports it.
