# fxmp guide

How to install and use fxmp. What it does in short: [README](../README.md).

## Installation

### macOS 12+ (Apple Silicon and Intel)
1. Download the `.dmg` (`fxmp_<version>_universal.dmg`), open it and
   drag the app into `Applications`.
2. The app is neither signed nor notarized, so macOS blocks it on first launch.
   Remove the quarantine flag:

   ```sh
   xattr -dr com.apple.quarantine "/Applications/fxmp.app"
   ```

3. Start the app. Its icon (a white play triangle over "fxmp") appears in the
   Dock, and the ⏭ glyph in the menu bar.

### Windows 10 and 11

Run `fxmp_<version>_x64-setup.exe` (or the `.msi`), or unpack
`fxmp_<version>_x64_portable.zip` anywhere (see [Portable mode](#portable-mode)).

### Android (8.0 and later, test builds)
`fxmp_<version>_android.apk` runs on 64-bit and 32-bit ARM phones. Copy it to
the phone, open it and allow the install from that source. It does what the
desktop app does with SMB libraries; what only a desktop has is left out (tray,
mini player, start at login, microphone, output device, local folders).
The phone shows the player alone, over the whole screen; the gear next to the
player buttons opens the other views (Genres, Playlist, Artists, Unreadable,
Settings, About), and the phone's Back button returns to the player.

- **Playing with the screen off:** a notification with previous / play-pause /
  next stays while music plays; it keeps the phone and its Wi-Fi awake for the
  music. On Android 13+ allow notifications when asked, or the controls do not
  show.
- **Passwords** are sealed with a key in the Android Keystore that never leaves
  the phone, and kept in the app's private data.
- The share must be reachable from the phone (same network or VPN).

## Where the app keeps its data

| What | macOS | Windows |
| --- | --- | --- |
| Settings (`settings.json`) | `~/Library/Application Support/hr.pvt.fxmp/` | `%APPDATA%\hr.pvt.fxmp\` |
| Library, playlist, bans, play history (`catalog.db`, with `catalog.db-wal`/`-shm`) | the same folder | the same folder |
| Passwords | keychain, service `fxmp` | Credential Manager (Windows credentials), service `fxmp` |
| Window cache (WebView) | `~/Library/WebKit/hr.pvt.fxmp/`, `~/Library/Caches/hr.pvt.fxmp/` | `%LOCALAPPDATA%\hr.pvt.fxmp\` |
| Log (`fxmp.log`, the previous one `fxmp.old.log`; a new file every 2 MB, so at most about 4 MB) | `~/Library/Logs/hr.pvt.fxmp/` (also in Console.app → Log Reports) | `%LOCALAPPDATA%\hr.pvt.fxmp\logs\` |

The log has no passwords. For more detail start the app with `RUST_LOG=debug`
(macOS, from Terminal: `RUST_LOG=debug /Applications/fxmp.app/Contents/MacOS/fxmp`;
the log then also shows in the Terminal).

### Portable mode

Put a folder named `fxmp-data` beside the app and fxmp keeps its settings,
library, playlist and log there instead of in the folders above: beside
`fxmp.exe` on Windows (the `fxmp_…_portable.zip` build comes with it, and the
`…-setup.exe` installer offers it at the end: answer **Yes** to "Portable
installation?", after picking the folder, e.g. on a USB stick), beside
`fxmp.app` on macOS. A copy on a USB stick or in any folder then carries its own
data, and the catalog works on any computer that reaches the same shares.

- Passwords stay in each computer's keychain / Credential Manager: on a new
  computer, enter them once in Settings. They are never written to the folder.
- The window cache (WebView) still goes to the system folder in the table above.
- "Start at login" registers the copy where it is now; turn it off before moving
  the copy.
- macOS: a downloaded app run straight from the disk image or the Downloads
  folder may run from a hidden random location (App Translocation) and not see
  the folder: move `fxmp.app` beside `fxmp-data` first.
- Delete or rename `fxmp-data` to go back to the normal folders.

A debug build with `SMP_DATA_DIR` keeps everything in that folder instead (see
[DEVELOPMENT.md](DEVELOPMENT.md#live-tests-and-testing)).

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
  (or a VPN coming up) recovers on its own. A share that does not answer is
  named in the status line ("Cannot connect to smb://10.0.0.5/Music"), with a
  **Retry** button that connects again at once.
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
  use "Play selected (n)". **All** plays the whole library. **Add to playlist
  (n)** (or right-click a genre → Add to playlist) mixes the genres into the
  playlist that is playing, without stopping it: their tracks land at random
  places among the ones still to come, tracks already in it are not added
  twice, and later reshuffles keep them.
  Only genres from Wikipedia's
  [List of ID3v1 genres](https://en.wikipedia.org/wiki/List_of_ID3v1_genres)
  are listed, with its names: the list has 192 codes (0–191), and 190 of them
  count as genres ("Other", code 12, and code 133, an offensive term, do not).
  Names are matched without regard to case or punctuation, with a few aliases
  such as "DnB" for "Drum & bass", and ID3 codes such as "(31)" or "(17)Rock"
  are read as their names. A few common subgenres go to their parent, e.g.
  "Deep House" to "House" (see `crates/core/src/genre.rs`). Tag values are split
  at `;`, `/`, `,` and ` | `, so "Rap/Hip-Hop" counts as Rap and Hip-hop; a
  name that is no genre as a whole counts by its parts ("Dance & House") or
  without "General " ("General Trance" is Trance).
  A track whose tag has no known genre takes the genre its top folder is named
  after, if that is a known genre (e.g. `DnB/…` gives Drum & bass). Otherwise
  the names in its tag are listed as they are, below the known genres under
  **Unsupported genres** (e.g. "Klapa"), so even those tracks show what kind of
  music they are; they play, add and merge like any genre. Only tracks with no
  genre, a placeholder ("Other", "Genre", "Various Artists") or an unknown code
  number are under **Unknown**.
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
  - **RND** puts the tracks still to come in a new random order; what played
    and the playing track stay where they are.
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
  (Settings → "Default lookup service", Last.fm unless changed); with "None"
  a click opens nothing (the "Look up on" menus still work).
- **Look** (Settings → Playback, kept per device): **Sliders** picks how the
  position and volume sliders look (Classic, Thin with a ring, Thick bar, Split
  bar, Pill volume), each in its own colour; **Player look** colours the player
  bar (Classic, Light, Night, Tonal). On Android the look also sizes the
  full-screen player.
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
  the length is unknown) with a position slider: drag it to jump forward or
  back in the track (FLAC/OGG land within 0.1 s, MP3 within about a second; the
  slider is off when the length is unknown). ⏮ previous (more than 3 s
  into a track it starts the track again), ⏯ pause, ⏭ next, 🚫 "never play this
  track again" (asks for confirmation: click again within 3 s), and a volume
  slider. Banned tracks come
  back with the **Restore** button in Settings.
- **Settings → Language**: English (default) or Hrvatski; it switches at once,
  the menu bar menu included.
- **Settings → Playback**: output device, crossfade on/off and its length
  (1–10 s), "Normalize volume", "Play stereo on all speakers (5.1/7.1)" (on by
  default: on a quad, 5.1 or 7.1 output the rear and side speakers get the left
  and right channel and the centre gets both, each 3 dB quieter; the subwoofer
  channel stays silent and the driver sends it the bass; off: only the front
  pair plays), "Resume playing automatically on start" (off by
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
  - **one left click = pause/resume**, **double click = next track** (a double
    click never also pauses). To tell the two apart, the pause waits for the
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
- **Microphone in use** (Settings → Playback): while an app uses the microphone
  (a call or a meeting), the music is lowered (default; to the "Lowered volume"
  level, 20 % unless set, 0–80 %), paused, left alone, or fxmp **asks**: a
  notification "Microphone in use" and, in the window (also the mini player), a
  bar with **Lower / Pause / Ignore** (desktop notifications cannot carry
  buttons; clicking it opens the window). It asks once per call. Once the
  microphone has been free for 3 s, the volume comes back, or the music plays
  again; music that was already paused, or that you played again by hand during
  the call, is left as it is. On Windows, "Only these apps" (e.g. `teams, zoom`)
  limits it to those apps; macOS cannot tell which app it is. Nothing is
  recorded and no microphone permission is asked: macOS reports whether the
  input device is running (the orange dot), Windows which apps use the
  microphone (its microphone icon). macOS 14.2 and later report which processes
  record, from any input (a call usually uses the headset's microphone, not the
  default input); older macOS versions only report whether the default input
  device runs, and cannot tell when it is also the default output (a headset).
- **Mini player**: the ▭ button next to 🚫 shrinks the window to the
  now-playing bar (title, position slider, ⏮ ⏯ ⏭ 🚫 and volume); ⤢ brings the
  full window back at its earlier size. It stays that way after a restart.
- **Closing the window** (red button, Alt+F4) keeps fxmp playing in the tray by
  default; Settings → "Closing the window keeps fxmp in the tray" off makes it
  quit instead.
- **F8** is pause/resume, **F9** next (Control Center works too). On Windows the
  keyboard's and headset's media keys (Play/Pause, Next, Previous) work too, and
  the track shows in the media overlay next to the volume flyout.
- **Dock**: the app has a Dock icon. Closing the window (red button) only hides
  it: the music plays on and the Dock and menu bar icons stay. A click on the
  Dock icon brings the window back. Minimizing (yellow button) puts the window
  in the Dock as usual. **Cmd+Q** (or Quit in the menu bar menu) quits.
- **Version**: the **About** tab (the last one) shows the version, the commit
  it was built from and the build date, with a link to the source code on
  GitHub (https://github.com/fx02/fxmp). A build without git (a source tarball)
  shows "unknown" as the commit.

## Known limitations

Planned work is listed in [ROADMAP.md](ROADMAP.md).

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
- The first start after an update that changes the library's format updates
  it before the window and the menu bar icon appear (about a second for
  ~59,000 tracks, once).
- A scan served by the backup server treats a file it already knows with the
  same size as unchanged, whatever its date: a file changed on the backup
  without changing size is read again only by a scan from the main server.
- Windows: the tray icon is the coloured app icon (the macOS menu bar has a
  black-and-white glyph the system recolours), so it shows on light and dark
  taskbars alike.
