# fxmp development

Building, tests and internals. Design: [specs/2026-10-24-smb-music-player-design.md](superpowers/specs/2026-10-24-smb-music-player-design.md).

## Building

### macOS

`cargo tauri build --target universal-apple-darwin`
(`rustup target add aarch64-apple-darwin x86_64-apple-darwin`); the `.dmg` ends
up in `target/universal-apple-darwin/release/bundle/dmg/`. The Rust version is
pinned in `rust-toolchain.toml`. The icons are generated with
`src-tauri/icons/gen_app_icon.py` + `cargo tauri icon`.

### Android

Building: JDK 17, the Android SDK with NDK 27 and the Rust Android targets
(`cargo tauri android init` installs them), then `scripts/build-android.sh`. It
signs with a key kept outside the repository (`FXMP_ANDROID_KEYS`, see the
script); an update must be signed with the same key.

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

A debug build never migrates from the installed old app's data. Without
`SMP_DATA_DIR` it does not migrate at all; with it, only from the folder in
`SMP_MIGRATE_FROM` (and from the old keychain service `smb-music-player-dev`),
so the migration can be tried on copies:

```sh
SMP_DATA_DIR=$(mktemp -d) SMP_MIGRATE_FROM=/path/to/a/copy cargo run -p fxmp
```
CI (`.github/workflows/build.yml`, on push to `main` and `feat/**` branches,
and manually) runs the tests and builds the macOS `.dmg` and the Windows
`.msi`/`.exe` as artifacts. Every action is pinned by commit SHA. The Windows
job may fail without failing the run until its first green build.

## SMB client

**Decision:** the pure-Rust crate [`smb`](https://crates.io/crates/smb) (smb-rs)
`=0.12.1`, in **async** mode, driven from a private tokio runtime
(`crates/core/src/source/smb.rs`). `libsmb2` over FFI was not needed.

**Spike (24 Oct 2026)** against both real servers (a Windows 10 PC
and an Unraid (Samba) NAS, one share with the music folder):

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
   a full walk (~59,400 files) ended with a `Timeout` error
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
  to the library, in transactions of 500. Measured on the Windows server, a first
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
