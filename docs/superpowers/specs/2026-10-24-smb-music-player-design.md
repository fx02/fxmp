# SMB Music Player — design (desktop: macOS + Windows)

Date: 2026-10-24
Status: approved for the macOS phase

## 1. Intent and rationale

The owner has a large, well-tagged music collection on a NAS (SMB share
`\\10.111.100.2\F\MP3`, protected by username and password), organized into
folders by artist or genre. They listen "by mood": sometimes trance, sometimes
chillout, jazz, local music, dnb, metal, oldies, and sometimes everything
together.

They need a small, unobtrusive desktop player that:
- scans the list of all songs on the disk once (or on demand) and remembers it,
- shows the music grouped by genre,
- plays random songs from the selected genre or combination of genres with one click,
- lives in the tray and skips a song with one click.

**Success:** click a genre and music plays within a couple of seconds. Every
time it starts with a different, truly random song. An unwanted song is skipped
with one click in the tray, without opening a window. The player stays out of
the way while it runs.

## 2. Goals

1. **Source:** SMB share. Host/IP, share and path can be changed in settings
   (default `10.111.100.2`, share `F`, path `MP3`). It also takes a username,
   password and an optional domain/workgroup.
2. **Index:** recursive tree scan and tag reading (artist, album, title, genre,
   duration, track number, ReplayGain if present). The result is stored in a
   local database (SQLite). The scan runs on first launch or manually
   ("Refresh"). Rescanning is incremental: it skips files whose size and mtime
   have not changed.
3. **Playback is streamed from SMB**, without downloading the whole collection.
   A song is read with a read-ahead buffer, and the next song starts loading
   before the current one ends.
   - Crossfade is **on by default, 5 s** (my decision).
   - When crossfade is off: there is no artificial silence between songs, but
     playback is not sample-accurate gapless (no trimming of the LAME encoder
     delay). If the decoder already supports it by itself, that is a bonus (my
     decision).
   - A manual "next" also does a short fade (~0.5–1 s) to avoid a click (my
     decision).
4. **Formats:** MP3, FLAC, OGG Vorbis (Opus in OGG is desirable, not required).
5. **Native audio:** decoding and output go through the native audio stack, not
   through web/HTML5 audio.
6. **Controls:** play/pause, next, volume, choice of output device among those
   the system offers (remembered, with fallback to the default if the device
   disappears). Crossfade can be turned on/off, duration is 1–10 s.
7. **Genres:** the main view is a list of genres with song counts. One or more
   genres can be selected, or "All". A click starts shuffle.
8. **True random:**
   - on every selection start, a fresh Fisher–Yates shuffle is done from a
     cryptographically seeded RNG (OS entropy), with no fixed seed and no
     stored order;
   - the first song is as random as the others; if it is the same as the first
     song of the previous start for the same selection, it is swapped with
     another one;
   - within one pass there are no repeats until the whole set is exhausted;
     then a new shuffle follows, taking care that the first song of the new pass
     is not the last one played.
   - A test confirms that the first songs are spread out across 1000 starts.
9. **Genre normalization:** similar names are merged automatically
   (case/whitespace/punctuation, e.g. "Drum & Bass" / "DnB" / "drum n bass",
   "Chill Out" / "Chillout"). The user can manually merge or rename a genre.
   Manual rules are stored locally and do not touch the tags on disk. A song
   with multiple genres (e.g. "Trance; Chillout" or multiple genre frames) is
   counted in each of them. The model has a separate song–genre table, and a
   multi-genre selection removes duplicates before shuffling (my decision).
   Songs without a genre go into "(no genre)", with a fallback to the parent
   folder name if it looks like a genre (how to detect that is the builder's
   decision).
10. **Unreadable files:** if a file is missing, corrupt, cannot be decoded, or
    the read times out, it is silently skipped and the next one plays. The file
    is flagged in the database and, after N consecutive failures, excluded from
    shuffle until a rescan is done.
11. **Loudness normalization (can be turned off, on by default):**
    - the ReplayGain tag (track gain) is applied if present;
    - if absent, loudness is measured during playback (EBU R128 / LUFS
      estimate), the result is stored in the database and used from the next
      time on;
    - there is no up-front loudness scan of the whole collection;
    - clipping protection (limiter or gain limit).
12. **Tray / menu bar:**
    - left click on the tray icon = **next song** (the main action);
    - right click opens a menu: current song (artist – title), pause/resume,
      next, a "Genre" submenu for quickly changing mood, "Show window", "Quit";
    - closing the window hides it to the tray instead of quitting the app.
    - Note for macOS: by convention, a left click on a menu bar icon opens the
      menu. If it cannot be reliably mapped to "next", two icons are used (⏭ and
      the main one) or ⏭ as the first menu item. The builder decides based on
      what the platform allows; the goal is skipping with one click.
13. **Media keys and OS controls:** play/pause/next from the keyboard,
    integration with macOS Now Playing and Windows SMTC.
14. **Notification on song change:** a short system notification
    (artist – title), can be turned off, off by default so it is not intrusive.
15. **Launch at login:** an option, off by default.
16. **Ban:** "Never play this song again" (from the tray and the window). Banned
    songs can be reviewed and unbanned in settings.
17. **Offline behavior:** if the share is unavailable, the catalog is still
    shown from the cache. The app clearly says the disk is unavailable and
    tries to reconnect. On a wrong password it asks for a correction.
18. **Credential security:** the password is stored in the OS keychain (macOS
    Keychain, Windows Credential Manager), never in a plain file or log.

## 3. Non-goals (for now)

- Android: a separate, second project after desktop, built on the same ideas
  (see §8).
- Editing tags on disk.
- Playlist management, ratings, statistics, last.fm, album covers as a required
  part (a cover may be shown if trivial).
- Copying or syncing the collection locally.
- Any cloud or server component.

## 4. Anti-goals (a miss even if it "works")

- The first song of a shuffle is repeatable or predictable.
- The app is heavy and noisy: high RAM/CPU in the background, pop-up windows,
  focus stealing, notifications by default.
- One bad file stops playback or throws a modal dialog.
- Password in plain text on disk.
- Launching the app rescans the whole disk.
- Playback depends on the OS having the share mounted (the app connects by
  itself over SMB).

## 5. Constraints

- **Order (owner's decision):** the macOS version is built and delivered first.
  Windows comes after. It will be built via GitHub Actions (Windows runner),
  and if that is not possible, the owner will later run Claude natively on a
  Windows machine. The code must be cross-platform from the start, with no
  macOS-only shortcuts in the core, but for Windows nothing is delivered in
  the first phase except a CI build, if it passes.

- **Servers:** the primary is `10.111.100.2`, Windows 10 (SMB 3.x, NTLMv2,
  signing possible). The backup is `10.111.0.2`, Unraid (Samba), with the same
  music. Port 445 on both is reachable from the development Mac, so testing is
  done against the real servers.
- **Primary and backup source (my decision):** settings support a primary and
  an optional backup server. Each has its own host, share, path and
  credentials. The catalog is built from the primary. A song is identified by
  its relative path within the music root. If the primary is unavailable,
  playback goes to the backup by the same relative path, and if the song is not
  there, it is skipped. The assumption is that the folder structure is the same
  on both; this must be confirmed on the real servers before implementing the
  fallback.

- macOS (Apple Silicon and Intel, macOS 12+) and Windows 10/11 x64.
- The collection likely has tens of thousands of files. The UI and database
  must handle that without stalls (virtualized lists).
- The network is a LAN. The buffer must survive short stalls of a few seconds.
- SMB2/3 with NTLM authentication.
- The development Mac currently has no Rust toolchain installed; it must be
  installed (rustup).

## 6. Technical decisions (my choice; the owner left the stack choice to me)

- **Framework:** Tauri 2 (Rust backend, lightweight web UI in the system
  webview). Gives a small binary, tray support and one codebase for both
  platforms.
- **Audio:** decoding in Rust (`symphonia`: MP3, FLAC, OGG Vorbis), output via
  `cpal` (CoreAudio / WASAPI), including device enumeration and selection. A
  custom mixer for crossfade (two decoded streams, equal-power curve), volume
  and gain.
- **SMB client:** a Rust SMB2/3 library that does not depend on OS mounting.
  The builder picks a specific library after a short maturity check. If none is
  reliable enough, the fallback is `libsmb2` via FFI.
- **Database:** SQLite (`rusqlite`), in the app data directory.
- **Credentials:** `keyring` crate.
- **UI:** minimal; the builder chooses the frontend framework (vanilla or
  Svelte is enough). Light/dark theme following the system.
- **Packaging:** `.dmg` for macOS, `.msi`/`.exe` for Windows. Signing and
  notarization are not required for personal use. The builder documents how to
  bypass Gatekeeper/SmartScreen.

## 7. Left to the builder

- Window layout and visual style, provided it is compact: one main view with
  genres, mini controls, a secondary artist/album view.
- Read-ahead buffer size and prefill strategy.
- The exact genre merging algorithm and "folder = genre" detection.
- N for excluding unreadable files (suggestion: 3).
- How to parallelize scanning and show progress.
- Icons.

## 8. Note for the future Android project

Same model: SMB index in a local database, shuffle by genre, crossfade,
skipping broken files. Suggested stack: Kotlin + Media3/ExoPlayer, `smbj` or
`jcifs-ng`, MediaSession (lock screen and notification controls), Android
Keystore. Shuffle and genre normalization must behave the same as on desktop,
so they are worth describing as a clear specification, not just as code.
