# SMB Music Player (macOS phase) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A lightweight macOS tray player that indexes music from an SMB share into a local SQLite catalog and plays it natively (MP3/FLAC/OGG) as a true shuffle by genre, with crossfade, loudness normalization and one-click skip from the tray.

**Architecture:** A Cargo workspace with two parts. `crates/core` (`smp-core`) is a platform- and UI-independent library: sources (local/SMB/failover), tag reading, catalog, genres, shuffle, audio engine (decoder → mixer → cpal output). `src-tauri` (`smb-music-player`) is the Tauri 2 app: settings, keychain, tray, media keys, Tauri commands/events and a UI in vanilla HTML/JS without a bundler (`ui/`). The core is tested without Tauri, and the audio engine is tested through a fake `Sink`.

**Tech Stack:** Rust (stable), Tauri 2, symphonia 0.5 (mp3, flac, ogg, vorbis), cpal, rubato, rtrb, lofty, rusqlite (bundled), rand, ebur128, keyring 3, souvlaki, tauri-plugin-notification, tauri-plugin-autostart, SMB client (chosen in Task 6).

**Spec:** `docs/superpowers/specs/2026-10-24-smb-music-player-design.md`

## Global Constraints

- This phase ships macOS only (12+, Apple Silicon and Intel), but `smp-core` must not contain macOS-only code. Platform-specific code lives only in `src-tauri`.
- Formats: MP3, FLAC, OGG Vorbis. Opus is not required.
- Crossfade: on/off, 1–10 s, default **on, 5 s**, equal-power curve. A manual "next" fades over **0.7 s**.
- Normalization: on by default. Uses the ReplayGain track gain if present, otherwise the measured loudness stored in the database. No upfront loudness scan. Target level **-18 LUFS**, gain is limited to **[-15, +6] dB**, and peaks to **-1 dBFS**.
- Shuffle: a fresh Fisher–Yates shuffle from an RNG seeded from OS entropy each time a selection is started. First track ≠ first track of the previous start of the same selection. No repeats within a pass. A new pass does not start with the last played track.
- An unreadable file is skipped silently. After **3** consecutive failures it is excluded from shuffle until the next scan.
- Passwords only in the keychain (service `smb-music-player`, account `{host}/{username}`). Never in a file or in the log.
- Settings defaults: primary server `10.111.100.2`, share `F`, path `MP3`. The secondary server is optional (suggested value in the UI: `10.111.0.2`). Notifications off, autostart off.
- Launching the app does not start a scan if the catalog exists. The app connects to SMB itself and does not rely on an OS mount.
- UI strings are in Croatian (the app's Croatian locale). This plan describes them in English.
- Integration tests against a real SMB server read `SMB_TEST_HOST`, `SMB_TEST_SHARE`, `SMB_TEST_PATH`, `SMB_TEST_USER`, `SMB_TEST_PASS` from the file `.env.test` (in `.gitignore`) and are marked `#[ignore]`.

## Review Focus

1. **SMB connection drops mid-track** (Wi-Fi, server asleep): playback from the buffer continues as long as it can. After a read timeout (10 s) the track is skipped instead of hanging forever. Test in Task 8 (`read_times_out_when_inner_stalls`) and Task 11 (`stalled_source_skips_track`).
2. **Large catalog** (50,000 tracks): the genre list and track selection for "All" return quickly, without blocking the UI. Test in Task 3 (`genres_query_fast_on_50k_tracks`).
3. **Output device disappears** (headphones/USB DAC unplugged): playback switches to the default device instead of going silent. Test in Task 11 (`device_error_falls_back_to_default`).
4. **Odd genre tags**: empty, whitespace only, ID3v1 numeric `(31)`, multiple values separated by `;`, `/` or `\0`, different letter case. Test in Task 2 (`split_and_normalize_edge_cases`).
5. **Rapid repeated clicks on "next"** and "next" during a crossfade: each click skips exactly one track, with no three-track overlap and no panic. Test in Task 11 (`rapid_next_skips_once_per_press`).

---

## File Structure

```
Cargo.toml                         workspace
.gitignore                         target/, .env.test, ui/node_modules
crates/core/Cargo.toml             smp-core
crates/core/src/lib.rs
crates/core/src/shuffle.rs         Shuffler
crates/core/src/genre.rs           split/normalize/alias/display
crates/core/src/catalog.rs         SQLite catalog
crates/core/src/source/mod.rs      MusicSource trait, FileEntry, ReadSeek
crates/core/src/source/local.rs    LocalSource (tests, dev)
crates/core/src/source/smb.rs      SmbSource
crates/core/src/source/failover.rs FailoverSource
crates/core/src/tags.rs            metadata reading (lofty)
crates/core/src/scan.rs            incremental scan
crates/core/src/audio/prefetch.rs  PrefetchReader (read-ahead buffer)
crates/core/src/audio/decoder.rs   symphonia + resampling → stereo f32
crates/core/src/audio/dsp.rs       crossfade curves, gain, ceiling, loudness
crates/core/src/audio/output.rs    cpal devices and Sink
crates/core/src/audio/player.rs    Player engine (thread, commands, events)
crates/core/tests/smb_live.rs      #[ignore] tests against a real server
src-tauri/Cargo.toml, tauri.conf.json, build.rs, icons/
src-tauri/src/main.rs              bootstrap, activation policy, plugins
src-tauri/src/settings.rs          Settings (JSON)
src-tauri/src/credentials.rs       keyring
src-tauri/src/state.rs             AppState (catalog, player, shuffler)
src-tauri/src/commands.rs          Tauri commands
src-tauri/src/tray.rs              tray icon and menu
src-tauri/src/media.rs             souvlaki (Now Playing, media keys)
ui/index.html, ui/style.css, ui/app.js
.github/workflows/build.yml        macOS + Windows CI build
README.md
```

---

### Task 1: Toolchain, workspace and Shuffler

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `crates/core/Cargo.toml`, `crates/core/src/lib.rs`, `crates/core/src/shuffle.rs`

**Interfaces:**
- Produces: `pub type TrackId = i64;` in `lib.rs`.
- Produces: `pub struct Shuffler<R: rand::Rng>` with
  - `pub fn new(pool: Vec<TrackId>, avoid_first: Option<TrackId>, rng: R) -> Self`
  - `pub fn next(&mut self) -> Option<TrackId>` (returns `None` if the pool is empty)
  - `pub fn remove(&mut self, id: TrackId)`
  - `pub fn first(&self) -> Option<TrackId>` (first track of the first pass, to be stored in `selection_state`)
- Produces: `pub fn entropy_shuffler(pool: Vec<TrackId>, avoid_first: Option<TrackId>) -> Shuffler<rand::rngs::StdRng>` (seeded from OS entropy).

- [ ] **Step 1: Install Rust if missing** — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`, then `source ~/.cargo/env && cargo --version`. Expected: a version is printed.
- [ ] **Step 2: Create the workspace** (`members = ["crates/core", "src-tauri"]`; add `src-tauri` to members only in Task 12, for now just `crates/core`) and `.gitignore`.
- [ ] **Step 3: Write failing tests** in `shuffle.rs`:

```rust
#[test] fn empty_pool_returns_none()            // next() == None
#[test] fn single_track_pool_repeats_forever()  // avoid_first == same id → still returns it
#[test] fn no_repeat_within_pass() {            // pool 0..100: first 100 next() calls = a permutation of 0..100
}
#[test] fn avoid_first_is_respected() {         // pool 0..10, avoid_first=Some(x), 500 times with entropy_shuffler: first() != x
}
#[test] fn first_track_distribution_is_uniform() {
    // pool 0..50, 1000 × entropy_shuffler(pool, None).next()
    // every id appears ≥1 time; none appears > 60 times
}
#[test] fn new_pass_does_not_start_with_last_played() {
    // pool 0..5, 2000 next() calls; at every pass boundary (every 5) element[i] != element[i-1]
}
#[test] fn remove_skips_removed_track()         // remove(3) → 3 no longer appears
```

- [ ] **Step 4:** `cargo test -p smp-core shuffle`. Expected: FAIL (compile error).
- [ ] **Step 5: Implement `Shuffler`.** Fisher–Yates is done by `rand::seq::SliceRandom::shuffle`. For avoid: if `order[0] == avoid` and `len > 1`, swap `order[0]` with `order[rng.gen_range(1..len)]`. The same applies at a new pass, where avoid is the last played track.
- [ ] **Step 6:** `cargo test -p smp-core shuffle`. Expected: PASS.
- [ ] **Step 7: Commit** `feat(core): workspace and true-random shuffler`

### Task 2: Genres (split, normalization, aliases)

**Files:**
- Create: `crates/core/src/genre.rs`

**Interfaces:**
- Produces:
  - `pub fn split_genres(raw: &str) -> Vec<String>` splits on `;`, `/`, `\0`, ` | `, trims and drops empty parts. ID3v1 `(N)` or a bare number `N` is mapped to a name via the lofty ID3v1 genre table.
  - `pub fn genre_key(name: &str) -> String`: lowercase, `&`/`'n'`/` n `/`+` → `and`, strips everything that is not alphanumeric, then applies the built-in aliases.
  - `pub const BUILTIN_ALIASES: &[(&str, &str)]` contains at least `("dnb","drumandbass")`, `("drumnbass","drumandbass")`, `("hiphop","hiphop")`, `("rnb","randb")`.
  - `pub fn pick_display(spellings: &[(String, usize)]) -> String` returns the most common original spelling; ties go to the first alphabetically.

- [ ] **Step 1: Failing tests:**

```rust
#[test] fn split_and_normalize_edge_cases() {
    assert_eq!(split_genres(""), Vec::<String>::new());
    assert_eq!(split_genres("   "), Vec::<String>::new());
    assert_eq!(split_genres("Trance; Chillout"), vec!["Trance","Chillout"]);
    assert_eq!(split_genres("Trance/Progressive"), vec!["Trance","Progressive"]);
    assert_eq!(split_genres("Jazz\0Blues"), vec!["Jazz","Blues"]);
    assert_eq!(split_genres("(31)"), vec!["Trance"]);
    assert_eq!(split_genres("31"), vec!["Trance"]);
}
#[test] fn keys_merge_common_variants() {
    for v in ["Drum & Bass","DnB","drum n bass","Drum'n'Bass","DRUM AND BASS"] {
        assert_eq!(genre_key(v), "drumandbass");
    }
    assert_eq!(genre_key("Chill Out"), genre_key("Chillout"));
    assert_ne!(genre_key("Trance"), genre_key("Techno"));
}
#[test] fn display_prefers_most_common_spelling() {
    assert_eq!(pick_display(&[("DnB".into(),3),("Drum & Bass".into(),40)]), "Drum & Bass");
}
```

- [ ] **Step 2:** `cargo test -p smp-core genre`. Expected: FAIL.
- [ ] **Step 3: Implement** in `genre.rs`.
- [ ] **Step 4:** `cargo test -p smp-core genre`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): genre splitting and normalization`

### Task 3: Catalog (SQLite)

**Files:**
- Create: `crates/core/src/catalog.rs`

**Interfaces:**
- Consumes: `TrackId`, `genre::{split_genres, genre_key, pick_display}`.
- Produces:
  - `pub struct TrackMeta { pub artist: Option<String>, pub album: Option<String>, pub title: Option<String>, pub track_no: Option<u32>, pub duration_ms: Option<u64>, pub raw_genre: Option<String>, pub rg_track_gain_db: Option<f32> }`
  - `pub struct TrackRow { pub id: TrackId, pub rel_path: String, pub meta: TrackMeta, pub measured_lufs: Option<f32> }`
  - `pub struct GenreCount { pub key: String, pub display: String, pub count: usize }`
  - `impl Catalog`:
    - `open(path: &Path) -> Result<Self>`
    - `open_in_memory() -> Result<Self>`
    - `is_empty() -> Result<bool>`
    - `file_unchanged(rel_path: &str, size: u64, mtime: i64) -> Result<bool>`
    - `upsert_track(rel_path: &str, size: u64, mtime: i64, meta: &TrackMeta) -> Result<TrackId>`
    - `begin_scan() -> Result<()>`: marks everything `present=0` and resets `fail_count=0`
    - `mark_seen(rel_path: &str) -> Result<()>`
    - `finish_scan() -> Result<usize>`: deletes `present=0` rows and returns the number deleted
    - `rebuild_genres() -> Result<()>`
    - `genres() -> Result<Vec<GenreCount>>`
    - `playable_tracks(genre_keys: &[String]) -> Result<Vec<TrackId>>`: empty means "All"; the result has no duplicates, no banned tracks and no `fail_count >= 3`
    - `track(id: TrackId) -> Result<TrackRow>`
    - `record_failure(id) -> Result<u32>`
    - `record_success(id) -> Result<()>`
    - `set_banned(id, bool) -> Result<()>`
    - `banned() -> Result<Vec<TrackRow>>`
    - `set_measured_lufs(id, f32) -> Result<()>`
    - `add_genre_rule(from_key: &str, to_display: &str) -> Result<()>`: merge and rename
    - `last_first(selection_key: &str) -> Result<Option<TrackId>>`
    - `set_last_first(selection_key: &str, id: TrackId) -> Result<()>`
    - `artists() -> Result<Vec<String>>`
    - `albums(artist: &str) -> Result<Vec<String>>`
    - `album_tracks(artist: &str, album: &str) -> Result<Vec<TrackId>>`: ordered by `track_no`, then by `rel_path`
  - `pub fn selection_key(genre_keys: &[String]) -> String`: sorted keys joined with `|`; empty → `"*"`.
- Schema: `tracks(id, rel_path UNIQUE, size, mtime, artist, album, title, track_no, duration_ms, raw_genre, rg_track_gain, measured_lufs, fail_count, banned, present)`, `genres(id, key UNIQUE, display)`, `track_genres(track_id, genre_id, PK(track_id, genre_id))` with an index on `genre_id`, `genre_rules(from_key PK, to_display)`, `selection_state(selection_key PK, last_first)`. Include `PRAGMA journal_mode=WAL`.
- Rule for `rebuild_genres`: genres come from `raw_genre` via `split_genres` → `genre_key` → `genre_rules` (if a rule exists, the key becomes `genre_key(to_display)` and the display is `to_display`). If a track has no genre and the first component of its `rel_path` has a `genre_key` equal to an existing key from tags, it gets that genre. Otherwise it goes into the key `"_none"` with the display `"(bez žanra)"` (Croatian locale string for "no genre").

- [ ] **Step 1: Failing tests:**

```rust
#[test] fn upsert_and_unchanged_detection()          // same size/mtime → true; different mtime → false
#[test] fn multi_genre_track_counts_in_each()        // "Trance; Chillout" → both have count 1
#[test] fn multi_genre_selection_dedups()            // playable_tracks(["trance","chillout"]).len()==1
#[test] fn empty_selection_means_all()
#[test] fn folder_fallback_and_none_bucket()          // "Jazz/x.mp3" without a tag + a "Jazz" tag exists somewhere → jazz; "Misc/y.mp3" → _none
#[test] fn three_failures_exclude_until_rescan()     // 3× record_failure → not in playable; begin_scan+mark_seen+finish_scan → it is back
#[test] fn banned_excluded_and_listed()
#[test] fn genre_rule_merges_and_renames()           // add_genre_rule("chillout","Chill") → a single genre "Chill" with both sets
#[test] fn finish_scan_removes_missing()
#[test] fn selection_key_is_order_independent()      // ["b","a"] == ["a","b"]; [] == "*"
#[test] fn genres_query_fast_on_50k_tracks() {
    // 50,000 upserts in one transaction across 40 genres, rebuild_genres
    // genres() and playable_tracks(&[]) each < 200 ms (release and debug)
}
```

- [ ] **Step 2:** `cargo test -p smp-core catalog`. Expected: FAIL.
- [ ] **Step 3: Implement `Catalog`.** Dependency: `rusqlite` with `bundled`. Add `pub fn transaction<T>(&mut self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T>` for bulk upserts.
- [ ] **Step 4:** `cargo test -p smp-core catalog`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): sqlite catalog`

### Task 4: MusicSource, LocalSource and tags

**Files:**
- Create: `crates/core/src/source/mod.rs`, `crates/core/src/source/local.rs`, `crates/core/src/tags.rs`
- Test fixtures: `crates/core/tests/fixtures/` with one short (1 s, silence or tone) `.mp3`, `.flac` and `.ogg` each, with tags (artist, title, genre `"Trance; Chillout"`, and the FLAC also with `REPLAYGAIN_TRACK_GAIN=-6.50 dB`), plus one `broken.mp3` (random bytes). Generated once (ffmpeg if available, otherwise the script `crates/core/tests/fixtures/make.sh`) and committed.

**Interfaces:**
- Produces:
  - `pub trait ReadSeek: std::io::Read + std::io::Seek + Send {}` with a blanket implementation
  - `pub struct FileEntry { pub rel_path: String, pub size: u64, pub mtime: i64 }`. `rel_path` always uses `/` and is relative to the music root.
  - `pub trait MusicSource: Send + Sync { fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<()>; fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>>; fn describe(&self) -> String; }`. `walk` returns only `.mp3`, `.flac` and `.ogg` (case-insensitive) and skips hidden files and folders.
  - `pub struct LocalSource { root: PathBuf }` and `LocalSource::new(root)`
  - `pub fn read_meta(reader: &mut dyn ReadSeek, ext: &str) -> Result<TrackMeta>` (lofty)

- [ ] **Step 1: Failing tests:**
  - `local_walk_finds_supported_only`: fixtures + `notes.txt` + `.hidden.mp3` → only the 4 audio files.
  - `read_meta_reads_tags`: artist, title and `raw_genre == "Trance; Chillout"` for all 3 formats, and the FLAC has `rg_track_gain_db == Some(-6.5)`.
  - `read_meta_broken_is_err`
- [ ] **Step 2:** `cargo test -p smp-core source tags`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run the tests again. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): music source trait, local source, tag reading`

### Task 5: Scanner

**Files:**
- Create: `crates/core/src/scan.rs`

**Interfaces:**
- Consumes: `MusicSource`, `Catalog`, `read_meta`.
- Produces:
  - `pub struct ScanProgress { pub seen: usize, pub read: usize, pub failed: usize, pub current: String }`
  - `pub struct ScanReport { pub total: usize, pub read: usize, pub unchanged: usize, pub failed: usize, pub removed: usize }`
  - `pub fn scan(source: &dyn MusicSource, catalog: &mut Catalog, progress: &mut dyn FnMut(&ScanProgress), cancel: &AtomicBool) -> Result<ScanReport>`
- Behavior:
  1. `begin_scan`, then `walk`.
  2. Unchanged files only get `mark_seen`. The others are opened, passed through `read_meta` and upserted.
  3. If `read_meta` fails, the file is still upserted with empty meta (it stays in the catalog and is skipped during playback) and is counted in `failed`.
  4. At the end, `finish_scan` and `rebuild_genres`.
  5. Commit in transactions of 500 files.
  6. Progress is reported at most every 100 ms.
  7. Cancel interrupts the walk and does not call `finish_scan`, so nothing is deleted.

- [ ] **Step 1: Failing tests** (against `LocalSource` on a tempdir copy of the fixtures):
  - `first_scan_reads_all`
  - `second_scan_reads_nothing_unchanged`: `read == 0`, `unchanged == 4`
  - `modified_file_is_reread`
  - `deleted_file_removed`
  - `broken_file_counted_not_fatal`
  - `cancel_keeps_catalog`
- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): incremental scanner`

### Task 6: SmbSource (spike, then implementation)

**Files:**
- Create: `crates/core/src/source/smb.rs`, `crates/core/tests/smb_live.rs`, `.env.test.example`

**Interfaces:**
- Produces:
  - `pub struct SmbConfig { pub host: String, pub share: String, pub root: String, pub username: String, pub password: String, pub domain: Option<String> }`
  - `SmbSource::connect(cfg: SmbConfig) -> Result<Self>` which implements `MusicSource`
  - `pub enum SourceError { Auth, Unreachable, NotFound, Io(String), Timeout }` with mapping from library errors. The UI distinguishes `Auth` from `Unreachable`.

- [ ] **Step 1: Prepare credentials.** Ask the owner to create `.env.test` (based on `.env.test.example`) with an SMB user for `10.111.100.2`. Do not ask for the password in chat.
- [ ] **Step 2: Spike (time-boxed, thrown away).** Try the pure-Rust crate `smb` (smb-rs). Against a real server it must: (a) list `F/MP3`, (b) read 1 MB from offset 0 and from an offset in the middle of a file, (c) return a distinct error for a wrong password. Repeat against `10.111.0.2` (Samba/Unraid). If the crate fails any of these points on either server, try `libsmb2` via FFI (static build from source in `build.rs`). Record the decision and the reason in the `README.md` section "SMB client".
- [ ] **Step 3: Failing tests** in `tests/smb_live.rs`, all `#[ignore]` and reading `.env.test`:
  - `live_walk_lists_audio`: at least 1 entry, all `rel_path` values without a leading `/`
  - `live_open_read_and_seek`: read the first 64 KiB, seek to `size/2`, read 64 KiB. The bytes match a second open.
  - `live_wrong_password_is_auth_error`
  - `live_scan_reads_tags_sample`: `scan` over `SmbSource`, cancelled after 200 files. `read > 0`, `failed < read`.
- [ ] **Step 4:** `cargo test -p smp-core --test smb_live -- --ignored`. Expected: FAIL.
- [ ] **Step 5: Implement `SmbSource`.** `open` returns a blocking `ReadSeek`. If the library is async, use an internal tokio runtime and `block_on`. The connection is shared. If the connection drops, reconnect once, then return the error. Every network operation has a 10 s timeout → `SourceError::Timeout`.
- [ ] **Step 6:** Run the tests again. Expected: PASS on both servers (change `SMB_TEST_HOST` manually).
- [ ] **Step 7: Commit** `feat(core): SMB source`

### Task 7: FailoverSource

**Files:**
- Create: `crates/core/src/source/failover.rs`

**Interfaces:**
- Produces: `pub struct FailoverSource { primary: Box<dyn MusicSource>, secondary: Option<Box<dyn MusicSource>> }`, which implements `MusicSource`.
  - `walk` goes through primary only. If primary returns `Unreachable`/`Timeout` and a secondary exists, it goes through the secondary.
  - `open` tries primary, and after `Unreachable`/`Timeout`/`NotFound` tries the secondary with the same `rel_path`.
  - `pub fn active(&self) -> Which` (`Primary`/`Secondary`) is used for the status in the UI.

- [ ] **Step 1: Failing tests** with a fake source (`FakeSource` in the test: configurable error per path):
  - `open_falls_back_on_unreachable`
  - `open_falls_back_on_not_found`
  - `auth_error_does_not_fall_back`
  - `walk_uses_secondary_when_primary_down`
  - `no_secondary_returns_primary_error`
- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5: Manual check** (using the `smb_live` test or an ad-hoc example): the same `rel_path` sample of 20 files exists on both servers. If it does not, note it in the README and tell the owner.
- [ ] **Step 6: Commit** `feat(core): primary/secondary failover source`

### Task 8: PrefetchReader

**Files:**
- Create: `crates/core/src/audio/prefetch.rs`

**Interfaces:**
- Produces: `pub struct PrefetchReader`, which implements `Read + Seek + Send`.
  - `PrefetchReader::new(inner: Box<dyn ReadSeek>, len: u64, cfg: PrefetchConfig) -> Self`
  - `pub struct PrefetchConfig { pub chunk: usize /*256 KiB*/, pub ahead: usize /*8 MiB*/, pub read_timeout: Duration /*10 s*/ }` with `Default`.
- Behavior:
  - A background thread reads ahead up to `ahead` bytes.
  - `read` blocks until data arrives or `read_timeout` expires. After expiry it returns `io::ErrorKind::TimedOut`.
  - A seek within the buffer does not call inner. A seek outside the buffer discards the buffer and repositions the thread.
  - `Drop` stops the thread.

- [ ] **Step 1: Failing tests** (inner is a `Cursor` over 5 MiB of random bytes, with a `SlowReader` wrapper):
  - `reads_match_source`: a full read = the source
  - `seek_forward_backward_match_source`
  - `prefetches_ahead`: after 50 ms with no reads the thread has read ≥ 1 MiB (check via a counter in `SlowReader`)
  - `read_times_out_when_inner_stalls`: inner blocks forever, `read_timeout` is 200 ms → `TimedOut` within 400 ms
  - `inner_error_propagates`
- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): prefetching reader`

### Task 9: Decoder

**Files:**
- Create: `crates/core/src/audio/decoder.rs`

**Interfaces:**
- Produces:
  - `pub struct Decoder`
  - `Decoder::open(reader: Box<dyn ReadSeek>, len: Option<u64>, ext: &str, out_rate: u32) -> Result<Self>`
  - `fn read(&mut self, out: &mut [f32]) -> Result<usize>`: interleaved stereo f32 at `out_rate`. `Ok(0)` means end of stream.
  - `fn duration_ms(&self) -> Option<u64>`
  - Mono is duplicated into L/R. For more than 2 channels use the downmix L = FL + 0.707·C (+ SL), R analogous, then normalize to avoid clipping.
  - If the source rate ≠ `out_rate`, rubato resamples (`FftFixedIn` or `SincFixedIn`).
- Symphonia: features `mp3`, `flac`, `ogg`, `vorbis`. `MediaSourceStream` over a wrapper that implements `MediaSource` (`is_seekable=true`, `byte_len=len`). Decoder errors within a packet (`DecodeError`) are skipped. `IoError` and an unexpected EOF before the first sample are `Err`.

- [ ] **Step 1: Failing tests** (fixtures from Task 4, each a 1 s tone at 44.1 kHz):
  - `decodes_mp3_flac_ogg`: `out_rate=48000` → about 48000 ± 2400 frames in total per file, RMS > 0.01
  - `broken_file_open_fails`
  - `mono_is_duplicated`: L == R
- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): symphonia decoder with resampling`

### Task 10: DSP (crossfade, gain, ceiling, loudness)

**Files:**
- Create: `crates/core/src/audio/dsp.rs`

**Interfaces:**
- Produces:
  - `pub fn equal_power(t: f32) -> (f32, f32)`: returns `(out_gain, in_gain) = (cos(t·π/2), sin(t·π/2))`, `t` is clamped to [0,1]
  - `pub fn track_gain_db(rg_track_gain_db: Option<f32>, measured_lufs: Option<f32>) -> f32`: RG if present, otherwise `-18 - lufs`, otherwise 0. The result is clamped to [-15, +6].
  - `pub fn db_to_lin(db: f32) -> f32`
  - `pub fn apply_ceiling(buf: &mut [f32])`: soft-knee limiter at -1 dBFS (0.891). Below 0.8 the signal is unchanged; above it compresses and never exceeds 0.891.
  - `pub struct LoudnessMeter`: `new(rate)`, `push(&[f32])`, `finish(self) -> Option<f32>` (integrated LUFS, `None` if less than 30 s was measured). Uses the `ebur128` crate.

- [ ] **Step 1: Failing tests:**

```rust
#[test] fn equal_power_endpoints_and_constant_power() {
    // t=0 → (1,0); t=1 → (0,1); for t∈{0.25,0.5,0.75}: o²+i² ≈ 1 (±1e-4)
}
#[test] fn gain_prefers_replaygain_then_lufs_then_zero() {
    // (Some(-6.5),Some(-10.0)) → -6.5; (None,Some(-10.0)) → -8.0; (None,None) → 0.0
    // (None,Some(-40.0)) → 6.0 (clamp); (Some(-30.0),None) → -15.0
}
#[test] fn ceiling_never_exceeds_and_is_transparent_below_knee() {
    // input 0.5 stays 0.5; inputs 1.0, 2.0, -3.0 → |y| ≤ 0.891
}
#[test] fn loudness_of_known_tone() {
    // 997 Hz sine, amplitude 0.1, stereo, 35 s, 48 kHz → LUFS ≈ -20.0 ± 1.0
    // 10 s of input → finish() == None
}
```

- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): dsp helpers`

### Task 11: Output and Player engine

**Files:**
- Create: `crates/core/src/audio/output.rs`, `crates/core/src/audio/player.rs`, `crates/core/src/audio/mod.rs`

**Interfaces:**
- Consumes: `MusicSource`, `PrefetchReader`, `Decoder`, `dsp::*`, `Shuffler`, `TrackId`.
- Produces (`output.rs`):
  - `pub fn list_output_devices() -> Vec<String>`
  - `pub fn default_output_device() -> Option<String>`
  - `pub trait Sink: Send { fn sample_rate(&self) -> u32; fn write(&mut self, stereo: &[f32]) -> SinkWrite; }` where `pub enum SinkWrite { Ok(usize) /*how many fit*/, DeviceLost }`
  - `pub struct CpalSink` and `CpalSink::open(device: Option<&str>) -> Result<(Self, String /*actual name*/)>`: an rtrb ring of about 200 ms between the engine thread and the cpal callback. Stereo is mapped onto the device channels (extra channels get silence). On error the callback sets a flag, so the next `write` returns `DeviceLost`.
  - `pub trait SinkFactory: Send { fn open(&mut self, device: Option<&str>) -> Result<(Box<dyn Sink>, String)>; }` with an implementation for cpal.
- Produces (`player.rs`):
  - `pub trait TrackProvider: Send { fn next_track(&mut self) -> Option<PlayItem>; fn on_failed(&mut self, id: TrackId); fn on_played(&mut self, id: TrackId, measured_lufs: Option<f32>); }`
  - `pub struct PlayItem { pub id: TrackId, pub rel_path: String, pub gain_db: f32 }`
  - `pub enum Command { Play(Box<dyn TrackProvider>), Pause, Resume, TogglePause, Next, SetVolume(f32 /*0..1*/), SetDevice(Option<String>), SetCrossfade { enabled: bool, secs: u8 }, SetNormalize(bool), Stop }`
  - `pub enum Event { TrackStarted { id: TrackId }, TrackSkipped { id: TrackId, reason: String }, Paused(bool), DeviceChanged(String), Idle }`
  - `pub struct Player` and `Player::spawn(source: Arc<dyn MusicSource>, sinks: Box<dyn SinkFactory>, events: Sender<Event>) -> Self`, with `fn send(&self, c: Command)`.
- Engine thread behavior:
  - Volume is applied as a linear gain with a 20 ms ramp.
  - Normalization multiplies by `db_to_lin(gain_db)` if enabled, followed by `apply_ceiling`.
  - A track is opened as `source.open` → `PrefetchReader` → `Decoder`. Opening the next track starts when `crossfade_secs + 10 s` remain in the current one (or 10 s when crossfade is off).
  - When `crossfade_secs` remain until the end, the crossfade starts (`equal_power`). Without crossfade, tracks follow one another with no inserted silence.
  - `Next` crossfades to the next track over 0.7 s. If the next track is not ready, a 0.3 s fade-out is done, then the next track starts. `Next` while a fade is already in progress immediately finishes the current fade (the old track is dropped) and starts a new one toward the next track, so never more than two tracks play at the same time.
  - Open error, `TimedOut` or a decode error before the first sample: `on_failed` + `TrackSkipped`, then the next track immediately. If 20 failures occur in a row, `Idle` is emitted and the engine waits.
  - When a track finishes normally: `on_played` with the LUFS from `LoudnessMeter` (measured before gain).
  - `SinkWrite::DeviceLost`: the default device is opened (`open(None)`) and `DeviceChanged` is emitted.
  - While paused, the engine does not write to the sink.

- [ ] **Step 1: Failing tests** (`FakeSink` records samples at 48 kHz and has a configurable `DeviceLost`, `FakeProvider` returns preset items, and the source is a `LocalSource` over the fixtures and generated 20 s tones, each with a different frequency for identification):
  - `plays_tracks_in_provider_order`
  - `broken_track_is_skipped_and_reported`: `TrackSkipped` for `broken.mp3`, and the next track plays
  - `stalled_source_skips_track`: `open` returns a reader that blocks, `read_timeout` is shortened to 200 ms in the test → `TrackSkipped`
  - `crossfade_overlaps_for_configured_secs`: crossfade of 2 s → the output has about 2 s ± 0.1 s where both frequencies are present (Goertzel)
  - `no_crossfade_means_no_gap`: no more than 50 ms of silence at the junction
  - `rapid_next_skips_once_per_press`: 5× `Next` within 50 ms → 5 `TrackStarted` events (each for a different item), never more than 2 frequencies at once in the output, no panic
  - `pause_stops_output`
  - `device_error_falls_back_to_default`: FakeSink returns `DeviceLost` → the factory gets `open(None)` and `DeviceChanged` arrives
  - `normalization_applies_gain`: `gain_db = -6` → RMS ≈ half (±10 %). With `SetNormalize(false)` the RMS is unchanged.
- [ ] **Step 2:** `cargo test -p smp-core player`. Expected: FAIL.
- [ ] **Step 3: Implement `output.rs` and `player.rs`.**
- [ ] **Step 4:** Run the tests again. Expected: PASS.
- [ ] **Step 5: Manual smoke test.** The example `crates/core/examples/play_local.rs <dir>` plays 3 tracks through `CpalSink` with a 5 s crossfade. The transition must be audible.
- [ ] **Step 6: Commit** `feat(core): audio output and player engine`

### Task 12: Tauri shell, settings, credentials, state and commands

**Files:**
- Create: `src-tauri/*` (`cargo tauri init` or by hand), `src-tauri/src/{main,settings,credentials,state,commands}.rs`, temporary `ui/index.html`
- Modify: `Cargo.toml` (add `src-tauri` to members)

**Interfaces:**
- Consumes: everything from `smp-core`.
- Produces:
  - `pub struct ServerConfig { pub host: String, pub share: String, pub root: String, pub username: String, pub domain: Option<String> }`
  - `pub struct Settings { pub primary: ServerConfig, pub secondary: Option<ServerConfig>, pub volume: f32, pub device: Option<String>, pub crossfade_enabled: bool, pub crossfade_secs: u8, pub normalize: bool, pub notify: bool, pub autostart: bool, pub last_selection: Vec<String> }` (serde). Defaults come from Global Constraints, and `volume` is 0.7. Saved to `app_config_dir()/settings.json`. `crossfade_secs` is clamped to 1..=10 on load.
  - `credentials::{get(host,user) -> Option<String>, set(host,user,pw) -> Result<()>}` via `keyring`.
  - `AppState`: `Mutex<Catalog>`, `Player`, `Arc<FailoverSource>` (can be recreated), connection status, `CatalogProvider` (a `TrackProvider` over `Catalog` + `Shuffler`, or over an album in order).
  - Commands (snake_case, all return `Result<_, String>`): `get_settings`, `save_settings(settings, primary_password: Option<String>, secondary_password: Option<String>)`, `test_connection(which: "primary"|"secondary") -> "ok"|"auth"|"unreachable"`, `start_scan`, `cancel_scan`, `get_genres -> Vec<GenreCount>`, `play_genres(keys: Vec<String>)`, `play_album(artist, album)`, `toggle_pause`, `next`, `set_volume(v)`, `list_devices`, `set_device(name: Option<String>)`, `set_crossfade(enabled, secs)`, `set_normalize(b)`, `ban_current`, `list_banned`, `unban(id)`, `merge_genre(from_key, into_display)`, `list_artists`, `list_albums(artist)`, `now_playing -> Option<NowPlaying{id, artist, title, album, genre_display, paused}>`.
  - Events to the UI: `now-playing`, `scan-progress`, `scan-done`, `connection-status`, `track-skipped`.
- Behavior:
  - `play_genres` builds `entropy_shuffler(catalog.playable_tracks(keys), catalog.last_first(selection_key(keys)))` and calls `set_last_first` on the first `next_track`.
  - `CatalogProvider::on_failed` calls `record_failure` and `shuffler.remove`.
  - At startup: if the catalog is empty and settings exist, a scan starts automatically. Otherwise it does not scan.
  - Scanning runs on a separate thread with its own `Catalog` connection (WAL).
  - Connection status: if the source is unavailable, the catalog and UI work from the cache. A reconnect is attempted in the background every 15 s, with a `connection-status` event (`ok` / `secondary` / `unreachable` / `auth`). Text for `unreachable` (Croatian locale): "Disk nije dostupan. Je li uključen WireGuard/VPN?" (English: "Disk is unavailable. Is WireGuard/VPN on?")
  - Test: `connection_status_maps_errors`, where `Unreachable` → `unreachable`, `Auth` → `auth`, and a fallback to secondary → `secondary`.

- [ ] **Step 1: Failing tests** (in `src-tauri`, pure logic without the Tauri runtime):
  - `settings_defaults_match_spec`: host `10.111.100.2`, share `F`, root `MP3`, crossfade on and 5, normalize true, notify false, autostart false
  - `settings_clamp_crossfade`: 0 → 1, 15 → 10
  - `settings_never_serialize_password`: the JSON does not contain `password`
  - `catalog_provider_records_last_first_and_failures`
- [ ] **Step 2:** `cargo test -p smb-music-player`. Expected: FAIL.
- [ ] **Step 3: Implement.** `tauri.conf.json`: `withGlobalTauri: true`, `frontendDist: "../ui"`, a single resizable `main` window of 420×640, identifier `hr.pvt.smb-music-player`.
- [ ] **Step 4:** Run the tests again. Expected: PASS. Then `cargo tauri dev`: the window opens and in the DevTools console `__TAURI__.core.invoke('get_settings')` returns the defaults.
- [ ] **Step 5: Commit** `feat(app): tauri shell, settings, keychain, commands`

### Task 13: UI

**Files:**
- Create/Modify: `ui/index.html`, `ui/style.css`, `ui/app.js`

**Interfaces:**
- Consumes: commands and events from Task 12.
- Content:
  - **Top bar** "now playing": artist – title, genre, ⏯, ⏭, 🚫 (ban with one-click confirmation) and a volume slider.
  - **Genres tab**: a list with track counts. Clicking a name plays that genre immediately, checkboxes + a "Play selected" button play multiple genres, and an "All" button plays everything. A filter field. Right-click or ⋯ on a genre offers "Merge into…/Rename".
  - **Artists tab**: search, a list (renders at most 300 results with a "narrow your search" message), click → albums, click an album → `play_album`.
  - **Settings tab**:
    - primary and secondary server (host, share, path, user, password, domain) with a "Test connection" button for each
    - output device dropdown
    - crossfade (toggle + 1–10 s slider)
    - normalization, notifications, launch at login
    - "Refresh catalog" with progress and cancel
    - list of banned tracks with "Restore"
  - Connection status: a green/yellow (secondary)/red dot next to the text.
  - The first launch without settings opens the Settings tab.
  - Style: compact, system font, light/dark theme via `prefers-color-scheme`.

- [ ] **Step 1: Implement.**
- [ ] **Step 2: Manual check** (`cargo tauri dev`, real server):
  1. Enter the settings and test the connection (ok). A wrong password must report "wrong username or password".
  2. Start a scan: progress grows, and when it finishes the genres appear with counts.
  3. Click a genre: it plays in under 3 s.
  4. Five restarts of the same genre give five different first tracks.
  5. ⏭ moves on with a short fade.
  6. A 10 s crossfade is audible at the natural end of a track.
  7. Changing the device moves the sound.
  8. Ban works and the track shows up in the settings.
  9. Restarting the app does not start a scan.
- [ ] **Step 3: Commit** `feat(ui): genres, artists, settings views`

### Task 14: Tray, media keys, notifications, autostart, window behavior

**Files:**
- Create: `src-tauri/src/tray.rs`, `src-tauri/src/media.rs`
- Modify: `src-tauri/src/main.rs`, `src-tauri/Cargo.toml`, `src-tauri/icons/tray-next.png` (template icon ⏭, 22 pt @1x/@2x)

**Interfaces:**
- Consumes: `AppState`, commands.
- Behavior:
  - **Tray:** `TrayIconBuilder` with the ⏭ icon as a template image (macOS) and `show_menu_on_left_click(false)`.
    - Left click (`TrayIconEvent::Click { button: Left, button_state: Up }`) does `next`.
    - Right click opens the menu:
      - an "Artist – Title" item (disabled, updated on `now-playing`)
      - "Pause"/"Resume" (the text changes)
      - "Next"
      - "Never play this track again"
      - a "Genre" submenu: "All" + the 30 largest genres by track count + other genres from `last_selection` if they are not among them
      - "Show window"
      - "Quit"
    - The tooltip shows the current track.
  - **Window:** `CloseRequested` → `prevent_close` + `hide`. On macOS `ActivationPolicy::Accessory`, so no Dock icon.
  - **Media keys and Now Playing:** `souvlaki::MediaControls`. Play/Pause/Toggle/Next map to commands, and the metadata is updated on `now-playing`.
  - **Notification** (`tauri-plugin-notification`) on `TrackStarted`, only if `notify` is true.
  - **Autostart** (`tauri-plugin-autostart`, LaunchAgent) follows the `autostart` setting.
  - **Single instance:** `tauri-plugin-single-instance`. A second launch only shows the window.

- [ ] **Step 1: Implement.**
- [ ] **Step 2: Manual check:**
  1. Close the window: music keeps playing, the icon is in the menu bar and not in the Dock.
  2. A single left click on the icon skips the track.
  3. Right click shows the menu with the current track. Changing the genre from the menu works.
  4. F8 (play/pause) and F9 (next) work, and the track is visible in Control Center.
  5. The notification appears only when enabled.
  6. Autostart on → after logging out and in, the app is in the tray.
  7. If a left click does not reliably arrive as `Click` on macOS, move "Next" to the first menu item and note it in the README (spec §2.12).
- [ ] **Step 3: Commit** `feat(app): tray, media keys, notifications, autostart`

### Task 15: Packaging, CI and README

**Files:**
- Create: `.github/workflows/build.yml`, `README.md` (or an addition)
- Modify: `src-tauri/tauri.conf.json` (bundle targets `dmg` and `app`, and for Windows `msi` and `nsis`)

- [ ] **Step 1:** `cargo tauri build --target universal-apple-darwin` (first `rustup target add aarch64-apple-darwin x86_64-apple-darwin`). Expected: a `.dmg` in `target/universal-apple-darwin/release/bundle/dmg/`.
- [ ] **Step 2: Manual check.** Install from the `.dmg`, launch it (the README describes `xattr -dr com.apple.quarantine "/Applications/SMB Music Player.app"` for the unsigned app) and repeat the checklists from Tasks 13 and 14 in short form.
- [ ] **Step 3: Workflow.** `build.yml`: matrix `macos-latest` and `windows-latest`, `dtolnay/rust-toolchain@stable`, `cargo test -p smp-core` (without ignored tests), `tauri-apps/tauri-action` builds, and the artifacts (`.dmg`, `.msi`/`.exe`) are uploaded. Triggered on push to `main` and on `workflow_dispatch`.
- [ ] **Step 4: README.** Describes: what the app does, installation, first-time setup, the SMB client (decision from Task 6), known limitations (unsigned, gapless is not sample-accurate) and how to run the live tests.
- [ ] **Step 5: Commit** `chore: packaging, CI workflow, README`
- [ ] **Step 6: Handoff.** Pushing to GitHub is an external action. Ask the owner for the repository (private) before pushing. The Windows build counts as successful only once CI passes and the owner has tried the installation.
