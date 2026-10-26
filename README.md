# fxmp — fx music player

Vibe coded with Claude

Formerly SMB Music Player.

A macOS-first Tauri 2 player that plays music straight from an SMB share
(MP3, FLAC, OGG Vorbis). Design: `docs/superpowers/specs/2026-10-24-smb-music-player-design.md`.

## What the app does

- Connects to the SMB server on its own (no OS mount needed), scans the music
  folder into a local library and groups the tracks by the genres in their tags.
- Plays shuffled from the selected genres: no repeats within a pass, and every
  start of a selection begins with a different track.
- Crossfade (on, 5 s, adjustable 1–10 s); a manual "next" does a short 0.7 s
  fade; volume normalization (ReplayGain or measured loudness, target -18 LUFS).
- Lives in the Dock and in the menu bar, with media keys and Now Playing,
  optional notifications and start at login.
- Optional backup server: if the main server does not answer, the backup is used.
- More music sources ("libraries"): further SMB servers and local folders (an
  external disk, a mounted network volume) play together with the main server.

## Installation (macOS 12+, Apple Silicon and Intel)

1. Download the `.dmg` (`fxmp_0.1.0_universal.dmg`), open it and
   drag the app into `Applications`.
2. The app is neither signed nor notarized, so macOS blocks it on first launch.
   Remove the quarantine flag:

   ```sh
   xattr -dr com.apple.quarantine "/Applications/fxmp.app"
   ```

3. Start the app. Its icon (a white play triangle over "SMB") appears in the
   Dock, and the ⏭ glyph in the menu bar.

Building from source: `cargo tauri build --target universal-apple-darwin`
(`rustup target add aarch64-apple-darwin x86_64-apple-darwin`); the `.dmg` ends
up in `target/universal-apple-darwin/release/bundle/dmg/`. The Rust version is
pinned in `rust-toolchain.toml`. The icons are generated with
`src-tauri/icons/gen_app_icon.py` + `cargo tauri icon`.

## Moving from SMB Music Player

fxmp was called SMB Music Player before. Quit SMB Music Player (menu bar menu →
Quit) before the first start of fxmp. On its first start fxmp brings over what
the old app had:

- **Files**: when fxmp has no settings and no library yet and the old app's
  folder (`~/Library/Application Support/hr.pvt.smb-music-player`, on Windows
  `%APPDATA%\hr.pvt.smb-music-player`) exists, fxmp copies everything in it
  (settings, library, playlist, bans) into its own folder (see
  [Where the app keeps its data](#where-the-app-keeps-its-data)). If the old app
  writes meanwhile, the copy is taken again. A file
  `migrated-from-smb-music-player` in fxmp's folder records that this was done,
  so it happens once.
- **Passwords**: each server's password is read from the old keychain service
  `smb-music-player` and written to `fxmp`, then read back. macOS asks whether
  fxmp may read each of the old app's entries (once per entry): choose
  **Allow** or **Always Allow**. If you refuse, nothing is lost: fxmp asks for
  that password in Settings, as for any password it does not have.
- **Start at login**: if it was on, fxmp sets up its own login item and removes
  the old app's (`~/Library/LaunchAgents/SMB Music Player.plist`; on Windows the
  `SMB Music Player` value under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`).

The old app's folder and keychain entries are not changed or deleted, so SMB
Music Player keeps working. Once fxmp works, you can remove the old app: drag
`SMB Music Player.app` to the Bin, delete its folder above, and delete its
keychain entries (Keychain Access, search for `smb-music-player`, or
`security delete-generic-password -s smb-music-player` once per entry). If fxmp
already had data of its own when it first started, nothing is copied; to copy
anyway, quit fxmp, delete its folder and start it again.

## Where the app keeps its data

| What | macOS | Windows |
| --- | --- | --- |
| Settings (`settings.json`) | `~/Library/Application Support/hr.pvt.fxmp/` | `%APPDATA%\hr.pvt.fxmp\` |
| Library, playlist, bans, play history (`catalog.db`, with `catalog.db-wal`/`-shm`) | the same folder | the same folder |
| Passwords | keychain, service `fxmp` | Credential Manager (Windows credentials), service `fxmp` |
| Window cache (WebView) | `~/Library/WebKit/hr.pvt.fxmp/`, `~/Library/Caches/hr.pvt.fxmp/` | `%LOCALAPPDATA%\hr.pvt.fxmp\` |
| Log | no file: written to the standard error output; to see it, start the app from Terminal: `/Applications/fxmp.app/Contents/MacOS/fxmp` (`RUST_LOG=debug` for more) | no file, and a release build has no console, so the log is not visible |

A debug build with `SMP_DATA_DIR` keeps everything in that folder instead (see
[Live tests and testing](#live-tests-and-testing)).

## First setup

On first launch a window opens. In **Settings** enter:

- **Server**, **Share**, **Path** (empty at first; the fields show examples
  such as `192.168.1.10`, `Music`, `MP3`),
- **Username** and **Password** (the domain is optional),
- optionally turn on **Backup server** and enter its details.

Click "Test connection", then "Save" and "Refresh library". Passwords are kept
only in the macOS keychain (service `fxmp`, one entry per server of
each library: account `library-{id}/{host}/{username}`), never in a file or a
log. macOS will ask whether the app may read the keychain: choose **Always Allow**.
Versions before libraries stored the account as `{host}/{username}`; on the
first start of this version those entries move to the main server's library
(the old entry is deleted only once the new one reads back the same, so macOS
may ask once more, for deleting it).

## Libraries (several music sources)

**Settings → Libraries** lists every music source. The first one is always the
main SMB server from the form above (with its optional backup server); it can
be renamed and switched off, but not removed. "Add SMB server…" and "Add local
folder…" add more, any number of them:

- **SMB server**: its own server, share, path, username, password and optional
  backup server, each with "Test connection". Its passwords go to the keychain
  under that library.
- **Local folder**: "Choose folder…" opens the system's folder picker (any
  folder: an external disk under `/Volumes`, a mounted network volume, a folder
  in your home). "Check folder" says whether it can be read. No password, and
  no keychain access at all.

Each library has a name (blank: "Main server", the folder's name or the
server's address), an **On** switch and (except the main one) **Remove**:

- Genres, shuffle, artists, the playlist and the Unreadable tab span every
  library that is on. A library switched off keeps its tracks in the catalog
  but hides them everywhere (and takes them out of the playlist); switching it
  on brings them back without a new scan.
- **Remove** (asks first) deletes the library's tracks from the catalog, with
  their bans, loudness and failures, and its passwords from the keychain. The
  music files are not touched.
- "Refresh library" scans every library that is on, one after another; the
  progress says which one ("USB (2/3): Scanning… 120 files"). A library that
  cannot be read (a disk not plugged in, a server down) is skipped and named in
  the result; its tracks are kept, never removed. An SMB library that had to
  be listed from its backup server (the main one was down) only gets new and
  changed files: nothing is removed until a refresh from the main server
  ("Scanned from the backup server, nothing removed: …"), since the backup may
  lag behind. A library whose folder lists no file at all although it has
  tracks (an empty mount point left behind, say) removes nothing either ("The
  folder looks empty, nothing removed: …"). If that library's music really is
  gone, remove the library (Settings → Libraries → Remove); for the main
  server, use Settings → Clear library (which empties the whole catalog) and
  refresh. A disk pulled out during a refresh
  stops that library's refresh: its files are not listed as unreadable.
- A local folder under `/Volumes/<name>` counts as there only while
  `/Volumes/<name>` is a mounted disk: an empty folder left there after an
  unclean eject (the disk then mounts as `<name> 1`) shows as "Music folder not
  found. Is the disk connected?" and is not scanned.
- The status dot shows the worst state of the libraries in use; its tooltip
  lists each one ("USB: Music folder not found. Is the disk connected?"). A
  library that is down is checked again every 15 s, so a disk plugged back in
  (or a VPN coming up) recovers on its own.
- On start, the saved playlist resumes once the library of its current track
  answers: an SMB server that does not answer yet (a VPN that comes up later
  at login) is waited for, with the saved track and position. It does not wait
  for a local folder that is missing (a disk not plugged in) while another
  library answers: the playlist goes on with what can be played. Pressing Play
  or Next while it waits, with another library answering, gives up the saved
  position and plays on at once.
- Play, Next and the menu bar icon are never silent: with an empty or
  played-out playlist they start a fresh shuffle of everything ("All"), and
  Next on a paused or cued track (the playlist restored paused on start)
  plays the next track.
- A removed library's id is never given to a new library.
- Each track plays from its own library. While a library is offline (and
  another one answers) its tracks in the playlist are passed over without
  being tried, however many come in a row; they are not counted
  towards the 3 failures that leave a track out of the shuffle (an unplugged
  disk does not get its tracks excluded).
- Local folders: hidden files and the macOS `._*` companion files are skipped,
  names with spaces and any letters (č, ć, ž, emoji…) work, linked folders
  outside the library are followed (a link to a folder inside it is not: its
  tracks keep their real path) and a link loop is not, and a name that is not
  valid Unicode is skipped (logged).
- The same file in two libraries (say, the NAS and a copy on a USB disk) is
  two tracks: duplicates are not merged in this version.
- The catalog keeps tracks by library and path. Updating to this version moves
  every existing track to the main server's library in place: bans, loudness,
  years, failures, genre merges, the shuffle history and the saved playlist all
  stay, and nothing is scanned again. Should that update fail, it is tried
  again at the next start (not on every refresh), and until then only the main
  server's library is refreshed.
- An exported playlist (`.m3u8`) lists each track's path relative to its own
  library's root, so a playlist mixing libraries only resolves for the tracks
  of the library whose folder it is saved in.
- Android is a separate project; the core library's local-folder source
  (`LocalSource` in `fxmp-core`) is what it will use to read music on the device.

## Usage

- **Genres** tab: clicking a genre plays only that genre. Tick several genres and
  use "Play selected (n)". **All** plays the whole library.
  Only genres from Wikipedia's
  [List of ID3v1 genres](https://en.wikipedia.org/wiki/List_of_ID3v1_genres)
  are listed, with its names: the list has 192 codes (0–191), and 190 of them
  count as genres ("Other", code 12, and code 133, an offensive term, do not).
  Names are matched without regard to case or punctuation, with a few aliases
  such as "DnB" for "Drum & bass", and ID3 codes such as "(31)" or "(17)Rock"
  are read as their names. A few common subgenres go to their parent, e.g.
  "Deep House" to "House" (see `crates/core/src/genre.rs`). Tag values are split
  at `;`, `/` and ` | `, so "Rap/Hip-Hop" counts as Rap and Hip-hop.
  A track whose tag has no known genre (no genre, "Other", or only unknown
  names) takes the genre its top folder is named after, if that is a known
  genre (e.g. `DnB/…` gives Drum & bass); otherwise it is under **Unknown**.
  Right-click (or ⋯) on a genre: "Merge into…" lists the genres that have
  tracks; typing in its search field also finds the other known genres.
  Merging a genre also moves the subgenres filed under it.
- **Playlist** tab: what plays, in play order. Clicking a genre (or "All", or
  several genres) fills it with that selection's shuffle; an album fills it in
  album order. The current track is highlighted, the tracks already played in
  this pass are dimmed, and each row shows "Artist – Title (Year)" and the
  length. Click a row to play it now: the playlist goes on from there in the
  same order (nothing is reshuffled). ↑ / ↓ move a track, × removes it.
  - Filters (text, genres, a year range and "Unknown year") only narrow what is
    shown; "Play filtered" starts a new shuffled playlist of just those tracks.
  - Tracks added by hand (marked +) play once per pass: when a shuffle starts
    its next pass, they are not in it. "New empty playlist" starts a hand-made
    list (the playing track plays on).
  - **Export…** saves the playlist (or, with "Export only filtered (N)", just the
    filtered rows) as an `.m3u8` file (UTF-8 Extended M3U, which Winamp,
    foobar2000, VLC, Music/iTunes, Kodi, Android players and most others read).
    The paths in it are relative to the music folder, so save the file in the
    root of the music folder on the share (e.g. `F/MP3/`); when the share is
    mounted under `/Volumes/<share>`, the save dialog starts there.
  - The playlist, the current track and where it stopped survive a restart: the
    app starts paused there, and Play continues from that spot. Settings →
    "Resume playing automatically on start" starts playing instead.
- **Artists** tab: search for an artist, click it to list the albums (with the
  year of their earliest track), click an album to play it in order. ▾ on an
  album lists its tracks; + on an album (or right-click) adds it to the
  playlist ("Play next" or "Add to end"); right-click or click a track for the
  same, one track at a time. Open artists and albums stay open while the
  library refreshes.
- **Right-click on a track** (Playlist, Artists, Unreadable, or the now-playing
  bar) opens the app's menu: "Copy “Artist – Title”" (with the year when it is
  known, e.g. `Metallica – Enter Sandman (1991)`), "Look up on" Last.fm,
  MusicBrainz, Discogs, Wikipedia, YouTube or Genius, "Look up artist", and
  where they apply "Play next", "Add to end", "Remove from playlist", "Never
  play again" and "Copy path". Only the artist and title are sent to the site.
  Clicking the title in the now-playing bar looks it up on the default service
  (Settings → "Default lookup service", Last.fm unless changed).
- The year is shown with the track wherever it appears: "Artist – Title (1991)"
  in the window, the menu bar menu and its tooltip. A track without a year in
  its tags shows none. The year comes from the date/year tag (the first
  4-digit year, 1900–2100).
- **Unreadable** tab (with the count): files that could not be played, or
  whose tags could not be read during a scan, with the full path on the server
  (`smb://host/share/path/file.mp3`, from the server it failed on, never with
  the username or password), the reason (file not found, timed out, server
  unreachable, cannot decode, unsupported format, tags unreadable, read error),
  how often it failed and when. "Copy path" / "Copy all" (one line per file:
  URL, tab, reason) put a valid, percent-encoded `smb://` URL on the
  clipboard; "Retry" / "Retry all" let the files play again. A track that
  failed 3 times in a row is skipped until the next refresh; the list keeps it
  until it plays, is retried, or a refresh reads it fine. Network trouble
  (timeouts, an unreachable server, a rejected password) is not the file's
  fault: it is not counted against the track and not listed. Banned tracks
  are not listed there (see Settings).
- A file whose tags cannot be read during a refresh keeps the tags, loudness
  and everything else it had. After network trouble (or a read error, or the
  file not being found) the next refresh tries it again; a file whose tags or
  audio are broken waits until it changes.
- A folder on the share that cannot be listed during a refresh is skipped and
  reported ("N folders could not be read; their tracks were kept"): its tracks
  stay in the library as they were (bans, loudness), instead of being removed.
  In a local-folder library the Unreadable tab shows the file's path (and
  "Copy path" gives its `file://` URL).
  A refresh that fails or is cancelled leaves the library as it was.
- Now-playing bar: "Artist – Title (Year)", the elapsed and total time
  (`m:ss / m:ss`, `h:mm:ss` for an hour or longer; only the elapsed time when
  the length is unknown) with a thin progress bar. ⏮ previous (more than 3 s
  into a track it starts the track again), ⏯ pause, ⏭ next, 🚫 "never play this
  track again" (asks for confirmation: click again within 3 s), and a volume
  slider. Banned tracks come
  back with the **Restore** button in Settings.
- **Settings → Language**: English (default) or Hrvatski; it switches at once,
  the menu bar menu included.
- **Settings → Playback**: output device, crossfade on/off and its length
  (1–10 s), "Normalize volume", "Resume playing automatically on start" (off by
  default, so a start at login does not start music unasked) and "Default
  lookup service".
- **Settings → Danger zone** (each asks for confirmation first):
  - **Reset settings to defaults**: volume, crossfade, output device,
    normalization, notifications, start at login (the login item is removed)
    and the last genre selection go back to their defaults; the language stays.
    The servers, libraries and passwords stay unless "Also forget servers and
    passwords" is ticked, which also deletes every library's passwords from the
    keychain and removes the added libraries with their tracks.
  - **Clear library**: stops a running scan and the music and empties the
    library (tracks, genres, genre merges, bans, unreadable list, shuffle
    history). Settings stay. "Refresh library" then reads the music again.
- **Notifications** (if turned on) show "Artist – Title" and the year, with no
  picture. macOS always draws the app's icon on a notification; an app cannot
  remove it.
- Menu bar icon:
  - **one left click = next track**, **double click = pause/resume** (a double
    click never also skips). To tell the two apart, "next" waits for the
    double-click time to pass: the system double-click interval, max 0.4 s
    (0.4 s on a default Mac; shorter if System Settings → Mouse/Trackpad →
    Double-click speed is faster; on Windows the system double-click time,
    0.25–0.9 s). So a single click reacts that much later than before.
  - Right click opens the menu: the current track; Pause, Previous, Next, Ban;
    **Look up current song** (on the default lookup site) and **Copy current
    song** ("Artist – Title (Year)"); Genre; **Volume** (0–100 % in steps of
    10, the current value checked, kept in step with the window's slider);
    **Output** ("System default" and every output device, the current one
    checked; the list is read again each time the menu opens); Show window;
    Quit; and, greyed out at the bottom, the version.
- **F8** is pause/resume, **F9** next (Control Center works too).
- **Dock**: the app has a Dock icon. Closing the window (red button) only hides
  it: the music plays on and the Dock and menu bar icons stay. A click on the
  Dock icon brings the window back. Minimizing (yellow button) puts the window
  in the Dock as usual. **Cmd+Q** (or Quit in the menu bar menu) quits.
- **Version**: Settings → **About** (at the bottom) shows the version, the
  commit it was built from and the build date, with a link to the source code
  on GitHub (https://github.com/fx02/fxmp) and **Copy version**,
  which copies e.g. "fxmp 0.1.0 (abc1234, 2026-10-25)" for bug
  reports. The same line is the last (greyed-out) item of the menu bar menu. A
  build without git (a source tarball) shows "unknown" as the commit.

## Translations

All user-facing texts, in the window and from the backend (menu bar menu,
error and status texts), live in `ui/locales/<code>.json`: flat `"area.name"`
keys, `{name}` placeholders, and plural texts as an object with one entry per
plural category of the language (`one`/`other` in English; `one`/`few`/`other`
in Croatian). English (`en.json`) is the fallback for any missing key. The
backend compiles the same files in (`src-tauri/src/i18n.rs`) and hands them to
the window, so each text exists once.

To add a language, e.g. German (`de`):

1. Copy `ui/locales/en.json` to `ui/locales/de.json` and translate every value;
   set `"language.name"` to the language's own name ("Deutsch").
2. Add `("de", include_str!("../../ui/locales/de.json"))` to `LOCALES` in
   `src-tauri/src/i18n.rs`.
3. Add the language's plural rule to `PLURAL_RULES` and its categories to
   `PLURAL_CATEGORIES` in `ui/lib.mjs`.
4. Run `node --test ui/*.test.mjs`: it checks that the new file has exactly the
   same keys, placeholders and plural categories as `en.json`.

The language then appears in Settings → Language.

## Known limitations

Planned work is listed in [docs/ROADMAP.md](docs/ROADMAP.md).

- The app is not signed (see `xattr` above).
- Gapless is not sample-accurate (the handover between tracks is not exact to the sample).
- Listing the folder on the server takes 15–40 s and shows no progress.
- The first full scan (~59,000 files) takes roughly 25–75 min, but genres
  appear while it runs.
- After updating to the version that shows years, the next "Refresh library"
  reads the tags of every file again (once), which takes about as long as a
  first scan. Bans, failure counts, genre merges and the shuffle history are
  kept, and the library stays playable meanwhile.
- If a track is played before you answer the keychain prompt, tracks are
  skipped until the password is available.
- Windows: CI builds the `.msi`/`.exe`, but media keys (souvlaki needs an HWND)
  are macOS-only for now, and the build has not been tried on real Windows.
  The code and tests were only type-checked for Windows on a Mac; the Windows
  CI job has not had a green run yet.
- The first start after an update that changes the library's format updates
  it before the window and the menu bar icon appear (about a second for
  ~59,000 tracks, once).
- A scan served by the backup server treats a file it already knows with the
  same size as unchanged, whatever its date: a file changed on the backup
  without changing size is read again only by a scan from the main server.
- Windows: the tray icon is the macOS template glyph (black on transparent), so
  it may be hard to see, or invisible, on a dark taskbar.

## Live tests and testing

```sh
cargo test --workspace            # without the live tests
node --test ui/*.test.mjs
cp .env.test.example .env.test    # fill in the details; the file is in .gitignore
cargo test -p fxmp-core --test smb_live -- --ignored --test-threads=1
cargo test -p fxmp live_ -- --ignored
```

Live tests (`cargo test -- --ignored`) only read from the server; nothing is written.

**Running a development build next to the installed app.** A debug build reads
`SMP_DATA_DIR`: when it is set, settings, the library and the playlist live in
that folder, passwords go to the keychain service `fxmp-dev`
instead of `fxmp`, and the build does not hand over to (or block) a
running installed app. Nothing of the installed app is read or written.
Release builds ignore the variable.

```sh
SMP_DATA_DIR=$(mktemp -d) cargo run -p fxmp
# afterwards: rm -rf that folder; security delete-generic-password -s fxmp-dev
```

Such a build migrates from SMB Music Player only from the folder in
`SMP_MIGRATE_FROM` (and from the old keychain service `smb-music-player-dev`),
never from the installed old app's data, so the migration can be tried on
copies:

```sh
SMP_DATA_DIR=$(mktemp -d) SMP_MIGRATE_FROM=/path/to/a/copy cargo run -p fxmp
```
CI (`.github/workflows/build.yml`, on push to `main` and `feat/**` branches,
and manually) runs the tests and builds the macOS `.dmg` and the Windows
`.msi`/`.exe` as artifacts. Every action is pinned by commit SHA. The Windows
job may fail without failing the run until its first green build.

## Owner checklist (manual checks)

These cannot be checked automatically (they need listening or clicking):

- [ ] Crossfade is audible and click-free at 1, 5 and 10 s (natural end of a track).
- [ ] ⏭ does a short 0.7 s fade, without a click.
- [ ] Gapless/album: the handover between tracks of an album is clean.
- [ ] Changing the output device moves the sound; unplugging headphones does not break playback.
- [ ] Volume normalization: tracks are about equally loud (on/off).
- [ ] Five starts of the same genre give five different first tracks.
- [ ] Look in light and dark mode (420×640, long titles, genre chip, ⋯).
- [ ] Left click on the menu bar icon reliably skips the track (if the click does
      not arrive reliably: move "Next" to the first menu item), after the
      system double-click interval, max 0.4 s (0.4 s on a default Mac).
- [ ] Double click on the menu bar icon pauses / resumes and does not skip;
      three quick clicks pause once (and then skip once).
- [ ] Menu bar menu → Volume: the checked step follows the window's slider, and
      picking a step changes the volume and moves the slider.
- [ ] Menu bar menu → Output: lists the devices (a newly plugged-in device shows
      up the next time the menu opens); picking one moves the sound and the
      Settings device list follows.
- [ ] Menu bar menu → Previous, Look up current song (opens the browser) and
      Copy current song (paste it somewhere); the greyed-out version line at the
      bottom matches Settings → About.
- [ ] Settings → About: the GitHub link opens the browser on the project page;
      Copy version puts the version line on the clipboard.
- [ ] The new app icon (play triangle over "SMB", no sound arcs) in the Dock,
      Finder, the app switcher (Cmd+Tab) and the notification picture; "SMB" is
      readable in small sizes (Finder list view).
- [ ] Right click: the menu opens, the track name updates, picking a genre plays that genre.
- [ ] Right-click on a genre in the window opens the app's own menu (not WebKit's),
      and "Merge into…" really merges.
- [ ] After closing the window, music keeps playing and the menu bar icon stays.
- [ ] The menu bar icon looks right in both the light and the dark menu bar.
- [ ] F8/F9 work and the track shows in Control Center.
- [ ] Notifications appear only when turned on (try with the installed .app).
- [ ] Start at login: after logging out and in, the app is in the menu bar with the window hidden.
- [ ] The Dock icon stays, and clicking it opens the window (also after the red
      close button and after minimizing); "Show window" and a second launch
      bring the window to the front; Cmd+Q quits.
- [ ] A full first scan to the end, with genres appearing along the way.
- [ ] The keychain prompt ("Always Allow"), and tracks play normally afterwards.
- [ ] Switching to the backup server when the main one is unreachable.
- [ ] Libraries: add a local folder on a USB disk with "Choose folder…", refresh,
      and play; unplug the disk (the dot turns red, the tooltip names it, its
      tracks are passed over and not listed as unreadable), plug it back in (green
      again within 15 s). Switch a library off and on; remove one (its tracks
      leave the genres). After updating, the keychain shows `library-1/…` entries
      and no old `{host}/{username}` ones, and the main server still connects.
- [ ] Genre chip, ban and "Restore" behave correctly during real listening.
- [ ] After the first "Refresh library" on this version, years appear (window,
      menu bar label and tooltip, albums) and bans are still there.
- [ ] Unreadable tab: "Copy path" and "Copy all" put the text on the clipboard;
      "Retry" removes a row; the count updates after a failed track.
- [ ] Playlist tab with a real "All" playlist: scrolling stays smooth, the current
      track is highlighted, clicking a row plays it, filters and "Play filtered" work.
- [ ] Quit while a track plays halfway, start again: the track and the time are
      shown paused; Play continues from there (and with "Resume playing
      automatically on start" on, it plays by itself with a short fade-in).
- [ ] ⏮ and the Previous media key: after 3 s the track starts again, before
      that the previous track plays.
- [ ] Export an `.m3u8` into the music folder root and open it in VLC (or
      another player): the songs play.
- [ ] Right-click "Look up on …" and a click on the now-playing title open the
      browser on the right site; "Copy" puts "Artist – Title (Year)" on the clipboard.
- [ ] During a library refresh, an open artist in the Artists tab stays open.
- [ ] Reset settings (with and without "Also forget servers and passwords") and
      Clear library behave as described, and the empty library offers "Refresh library".

## SMB client

**Decision:** the pure-Rust crate [`smb`](https://crates.io/crates/smb) (smb-rs)
`=0.12.1`, in **async** mode, driven from a private tokio runtime
(`crates/core/src/source/smb.rs`). `libsmb2` over FFI was not needed.

**Spike (24 Oct 2026)** against both real servers (Windows 10 `10.111.100.2`
and Unraid/Samba `10.111.0.2`, share `F`, path `MP3`):

| Criterion | Windows 10 | Samba |
|---|---|---|
| listing `F/MP3` | yes | yes |
| 1 MB from offset 0 and from the middle of a file | yes | yes (see below) |
| wrong password → its own error (`Logon Failure 0xC000006D`) | yes | yes |

Three problems found in the crate along the way, and how they were solved:

1. **The synchronous (`multi_threaded`) mode loses data.** The receive loop reads
   the socket with a 100 ms timeout and `read_exact`; when a large READ response
   arrives in pieces more slowly than that, the bytes already read are thrown
   away, the stream falls apart ("bad magic") and the request times out after
   10 s. The async (tokio) backend does not have this problem, so it is used,
   and `MusicSource` stays blocking through `block_on`. Consequence: `SmbSource`
   methods must not be called from an async context (use `spawn_blocking`).
2. **No `TCP_NODELAY`.** Every request is sent in several small writes, so
   Nagle + delayed ACK add ~40–80 ms per request. `smb-transport` 0.12.1 is
   therefore vendored in `vendor/smb-transport` with one added line
   (`set_nodelay(true)`), see `vendor/smb-transport/SMP-PATCH.md`. Walking
   ~10,900 files: 255 s without the patch, 70 s with it.
3. **Listing a folder sometimes hangs until the timeout.** The `Directory::query`
   stream in `smb` 0.12.1 loses a wake-up (`notify_waiters` instead of
   `notify_one` in `poll_next`), so it does not send the final query and waits
   until the timeout, followed by a reconnect and a retry. On Samba
   (`10.111.0.2`) a full walk (~59,400 files) ended with a `Timeout` error
   because of this. `smb` 0.12.1 is therefore vendored in `vendor/smb` with that
   one change, see `vendor/smb/SMP-PATCH.md`. Full Samba walk: `Timeout` after
   130 s without the patch, ~38 s without errors with it.

Other notes:
- Reads go in 256 KiB chunks (a read-ahead buffer per open file); a whole file
  is never downloaded.
- Every network operation has a 10 s timeout (`SourceError::Timeout`), except
  folder listing, which has 15 s. After a broken connection or a timeout the
  connection is re-established once (e.g. a half-open connection after sleep),
  and only then is the error returned.
- A subfolder that cannot be listed for a reason of its own (access denied,
  gone, I/O error) is skipped with a log entry. A dropped connection or a
  timeout aborts the whole walk with an error, otherwise the scanner would delete
  the tracks of the folders it did not walk. Failing to list the root is an
  error too.
- The walk lists up to 8 folders at once on the same connection (each on its own
  thread), so the order of the files found is not fixed. The error rules above
  apply the same way.
- The scanner (`scan`) opens new/changed files and reads their tags on 8
  parallel threads (`ScanOptions { workers }`); only the calling thread writes
  to the library, in transactions of 500. Measured on `10.111.100.2`, a first
  scan of 500 files: 28.0 s with one thread, 9.8 s with eight (2.8×).
  During a scan the genres are rebuilt after every 5000 new files or every
  30 s, whichever comes first, so the library is usable before the first scan
  ends.

Live tests (read-only, nothing is written to the servers):

```sh
cp .env.test.example .env.test   # fill in the details; the file is in .gitignore
cargo test -p fxmp-core --test smb_live -- --ignored --test-threads=1
```

The tests go to the main server and, if `SMB_TEST2_HOST` is set, also to the backup.
