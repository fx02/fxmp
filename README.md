# SMB music player

A macOS-first Tauri 2 player that plays music straight from an SMB share
(MP3, FLAC, OGG Vorbis). Design: `docs/superpowers/specs/2026-10-24-smb-music-player-design.md`.

## What the app does

- Connects to the SMB server on its own (no OS mount needed), scans the music
  folder into a local library and groups the tracks by the genres in their tags.
- Plays shuffled from the selected genres: no repeats within a pass, and every
  start of a selection begins with a different track.
- Crossfade (on, 5 s, adjustable 1–10 s); a manual "next" does a short 0.7 s
  fade; volume normalization (ReplayGain or measured loudness, target -18 LUFS).
- Lives in the menu bar (no Dock icon), with media keys and Now Playing,
  optional notifications and start at login.
- Optional backup server: if the main server does not answer, the backup is used.

## Installation (macOS 12+, Apple Silicon and Intel)

1. Download the `.dmg` (`SMB Music Player_0.1.0_universal.dmg`), open it and
   drag the app into `Applications`.
2. The app is neither signed nor notarized, so macOS blocks it on first launch.
   Remove the quarantine flag:

   ```sh
   xattr -dr com.apple.quarantine "/Applications/SMB Music Player.app"
   ```

3. Start the app. Its icon appears in the menu bar.

Building from source: `cargo tauri build --target universal-apple-darwin`
(`rustup target add aarch64-apple-darwin x86_64-apple-darwin`); the `.dmg` ends
up in `target/universal-apple-darwin/release/bundle/dmg/`. The Rust version is
pinned in `rust-toolchain.toml`. The icons are generated with
`src-tauri/icons/gen_app_icon.py` + `cargo tauri icon`.

## First setup

On first launch a window opens. In **Settings** enter:

- **Server**, **Share**, **Path** (defaults: `10.111.100.2`, `F`, `MP3`),
- **Username** and **Password** (the domain is optional),
- optionally turn on **Backup server** (suggested `10.111.0.2`) and enter its details.

Click "Test connection", then "Save" and "Refresh library". Passwords are kept
only in the macOS keychain (service `smb-music-player`), never in a file or a log.
macOS will ask whether the app may read the keychain: choose **Always Allow**.

## Usage

- **Genres** tab: clicking a genre plays only that genre. Tick several genres and
  use "Play selected (n)". **All** plays the whole library.
  Only the 192 ID3v1 genres are listed, named as in Wikipedia's
  [List of ID3v1 genres](https://en.wikipedia.org/wiki/List_of_ID3v1_genres)
  (matched without regard to case or punctuation, with a few aliases such as
  "DnB" for "Drum & bass"). A few common subgenres go to their parent, e.g.
  "Deep House" to "House" (see `crates/core/src/genre.rs`). Tracks with no known
  genre in the tag or in the top folder name, or tagged "Other", are under
  **Unknown**.
  Right-click (or ⋯) on a genre: "Merge into…" picks another known genre from a
  filterable list.
- **Artists** tab: search for an artist, click it to list the albums, click an
  album to play it in order.
- Window controls: ⏯ pause, ⏭ next, 🚫 "never play this track again" (asks for
  confirmation: click again within 3 s), and a volume slider. Banned tracks come
  back with the **Restore** button in Settings.
- **Settings → Language**: English (default) or Hrvatski; it switches at once,
  the menu bar menu included.
- **Settings → Playback**: output device, crossfade on/off and its length
  (1–10 s), and "Normalize volume".
- Menu bar: **left click on the icon = next track**; right click opens the menu
  (current track, pause, genre, ban, show window, quit).
- **F8** is pause/resume, **F9** next (Control Center works too).
- Closing the window does not quit the app; quit from the menu bar menu.

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

- The app is not signed (see `xattr` above).
- Gapless is not sample-accurate (the handover between tracks is not exact to the sample).
- Listing the folder on the server takes 15–40 s and shows no progress.
- The first full scan (~59,000 files) takes roughly 25–75 min, but genres
  appear while it runs.
- If a track is played before you answer the keychain prompt, tracks are
  skipped until the password is available.
- Windows: CI builds the `.msi`/`.exe`, but media keys (souvlaki needs an HWND)
  are macOS-only for now, and the build has not been tried on real Windows.
- Windows: the tray icon is the macOS template glyph (black on transparent), so
  it may be hard to see, or invisible, on a dark taskbar.

## Live tests and testing

```sh
cargo test --workspace            # without the live tests
node --test ui/*.test.mjs
cp .env.test.example .env.test    # fill in the details; the file is in .gitignore
cargo test -p smp-core --test smb_live -- --ignored --test-threads=1
cargo test -p smb-music-player live_ -- --ignored
```

Live tests (`cargo test -- --ignored`) only read from the server; nothing is written.
CI (`.github/workflows/build.yml`, on push to `main` and manually) runs the tests
and builds the macOS `.dmg` and the Windows `.msi`/`.exe` as artifacts.

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
      not arrive reliably: move "Next" to the first menu item).
- [ ] Right click: the menu opens, the track name updates, picking a genre plays that genre.
- [ ] Right-click on a genre in the window opens the app's own menu (not WebKit's),
      and "Merge into…" really merges.
- [ ] After closing the window, music keeps playing and the menu bar icon stays.
- [ ] The menu bar icon looks right in both the light and the dark menu bar.
- [ ] F8/F9 work and the track shows in Control Center.
- [ ] Notifications appear only when turned on (try with the installed .app).
- [ ] Start at login: after logging out and in, the app is in the menu bar with the window hidden.
- [ ] No Dock icon; "Show window" and a second launch bring the window to the front.
- [ ] A full first scan to the end, with genres appearing along the way.
- [ ] The keychain prompt ("Always Allow"), and tracks play normally afterwards.
- [ ] Switching to the backup server when the main one is unreachable.
- [ ] Genre chip, ban and "Restore" behave correctly during real listening.

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
cargo test -p smp-core --test smb_live -- --ignored --test-threads=1
```

The tests go to the main server and, if `SMB_TEST2_HOST` is set, also to the backup.
