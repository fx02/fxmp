# SMB Music Player (macOS faza) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lagani macOS tray player koji indeksira glazbu sa SMB sharea u lokalni SQLite katalog i pušta je nativno (MP3/FLAC/OGG) kao pravi shuffle po žanru, s crossfadeom, normalizacijom glasnoće i preskakanjem jednim klikom u trayu.

**Architecture:** Cargo workspace s dvije cjeline. `crates/core` (`smp-core`) je biblioteka neovisna o platformi i UI-ju: izvori (lokalni/SMB/failover), čitanje tagova, katalog, žanrovi, shuffle, audio engine (dekoder → mikser → cpal izlaz). `src-tauri` (`smb-music-player`) je Tauri 2 aplikacija: postavke, keychain, tray, medijske tipke, Tauri komande/eventovi i UI u vanilla HTML/JS bez bundlera (`ui/`). Jezgra se testira bez Taurija, a audio engine se testira preko lažnog `Sink`-a.

**Tech Stack:** Rust (stable), Tauri 2, symphonia 0.5 (mp3, flac, ogg, vorbis), cpal, rubato, rtrb, lofty, rusqlite (bundled), rand, ebur128, keyring 3, souvlaki, tauri-plugin-notification, tauri-plugin-autostart, SMB klijent (odabire se u Tasku 6).

**Spec:** `docs/superpowers/specs/2026-10-24-smb-music-player-design.md`

## Global Constraints

- Ova faza isporučuje samo macOS (12+, Apple Silicon i Intel), ali `smp-core` ne smije imati macOS-only kod. Platformski specifičan kod živi samo u `src-tauri`.
- Formati: MP3, FLAC, OGG Vorbis. Opus nije obavezan.
- Crossfade: uključi/isključi, 1–10 s, default **uključen, 5 s**, equal-power krivulja. Ručni "sljedeća" radi fade od **0,7 s**.
- Normalizacija: default uključena. Koristi ReplayGain track gain ako postoji, inače izmjerenu glasnoću spremljenu u bazu. Bez unaprijednog skeniranja glasnoće. Ciljna razina **-18 LUFS**, pojačanje se ograničava na **[-15, +6] dB**, a vrhovi na **-1 dBFS**.
- Shuffle: svježe Fisher–Yates miješanje iz OS-entropijom seedanog RNG-a pri svakom pokretanju odabira. Prva pjesma ≠ prva pjesma prethodnog pokretanja istog odabira. Unutar prolaza nema ponavljanja. Novi prolaz ne počinje zadnje odsviranom pjesmom.
- Nečitljiva datoteka se tiho preskače. Nakon **3** uzastopna neuspjeha isključuje se iz shufflea do sljedećeg skeniranja.
- Lozinke samo u keychainu (servis `smb-music-player`, račun `{host}/{username}`). Nikad u datoteci ni u logu.
- Defaulti postavki: glavni server `10.111.100.2`, share `F`, putanja `MP3`. Rezervni server je opcionalan (predložena vrijednost u UI-ju: `10.111.0.2`). Obavijesti isključene, autostart isključen.
- Pokretanje aplikacije ne pokreće skeniranje ako katalog postoji. Aplikacija se sama spaja na SMB i ne oslanja se na OS mount.
- UI tekstovi su na hrvatskom.
- Integracijski testovi prema stvarnom SMB-u čitaju `SMB_TEST_HOST`, `SMB_TEST_SHARE`, `SMB_TEST_PATH`, `SMB_TEST_USER`, `SMB_TEST_PASS` iz datoteke `.env.test` (u `.gitignore`) i označeni su `#[ignore]`.

## Review Focus

1. **SMB veza pukne usred pjesme** (Wi-Fi, server spava): reprodukcija iz buffera nastavlja koliko može. Nakon timeouta čitanja (10 s) pjesma se preskače, a ne visi beskonačno. Test u Tasku 8 (`read_times_out_when_inner_stalls`) i Tasku 11 (`stalled_source_skips_track`).
2. **Velik katalog** (50 000 pjesama): popis žanrova i odabir pjesama za "Sve" vraćaju se brzo, bez blokiranja UI-ja. Test u Tasku 3 (`genres_query_fast_on_50k_tracks`).
3. **Izlazni uređaj nestane** (izvučene slušalice/USB DAC): reprodukcija se prebaci na zadani uređaj, a ne utihne. Test u Tasku 11 (`device_error_falls_back_to_default`).
4. **Čudni žanr tagovi**: prazan, samo razmaci, ID3v1 numerički `(31)`, više vrijednosti s `;`, `/` ili `\0`, različita velika/mala slova. Test u Tasku 2 (`split_and_normalize_edge_cases`).
5. **Brzo uzastopno klikanje "sljedeća"** i "sljedeća" tijekom crossfadea: svaki klik preskače točno jednu pjesmu, bez preklapanja tri pjesme i bez panike. Test u Tasku 11 (`rapid_next_skips_once_per_press`).

---

## File Structure

```
Cargo.toml                         workspace
.gitignore                         target/, .env.test, ui/node_modules
crates/core/Cargo.toml             smp-core
crates/core/src/lib.rs
crates/core/src/shuffle.rs         Shuffler
crates/core/src/genre.rs           split/normalize/alias/display
crates/core/src/catalog.rs         SQLite katalog
crates/core/src/source/mod.rs      MusicSource trait, FileEntry, ReadSeek
crates/core/src/source/local.rs    LocalSource (testovi, dev)
crates/core/src/source/smb.rs      SmbSource
crates/core/src/source/failover.rs FailoverSource
crates/core/src/tags.rs            čitanje metapodataka (lofty)
crates/core/src/scan.rs            inkrementalno skeniranje
crates/core/src/audio/prefetch.rs  PrefetchReader (read-ahead buffer)
crates/core/src/audio/decoder.rs   symphonia + resampling → stereo f32
crates/core/src/audio/dsp.rs       crossfade krivulje, gain, ceiling, loudness
crates/core/src/audio/output.rs    cpal uređaji i Sink
crates/core/src/audio/player.rs    Player engine (thread, komande, eventovi)
crates/core/tests/smb_live.rs      #[ignore] testovi na stvarnom serveru
src-tauri/Cargo.toml, tauri.conf.json, build.rs, icons/
src-tauri/src/main.rs              bootstrap, activation policy, plugins
src-tauri/src/settings.rs          Settings (JSON)
src-tauri/src/credentials.rs       keyring
src-tauri/src/state.rs             AppState (catalog, player, shuffler)
src-tauri/src/commands.rs          Tauri komande
src-tauri/src/tray.rs              tray ikona i izbornik
src-tauri/src/media.rs             souvlaki (Now Playing, medijske tipke)
ui/index.html, ui/style.css, ui/app.js
.github/workflows/build.yml        macOS + Windows CI build
README.md
```

---

### Task 1: Toolchain, workspace i Shuffler

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `crates/core/Cargo.toml`, `crates/core/src/lib.rs`, `crates/core/src/shuffle.rs`

**Interfaces:**
- Produces: `pub type TrackId = i64;` u `lib.rs`.
- Produces: `pub struct Shuffler<R: rand::Rng>` s
  - `pub fn new(pool: Vec<TrackId>, avoid_first: Option<TrackId>, rng: R) -> Self`
  - `pub fn next(&mut self) -> Option<TrackId>` (vraća `None` ako je pool prazan)
  - `pub fn remove(&mut self, id: TrackId)`
  - `pub fn first(&self) -> Option<TrackId>` (prva pjesma prvog prolaza, za spremanje u `selection_state`)
- Produces: `pub fn entropy_shuffler(pool: Vec<TrackId>, avoid_first: Option<TrackId>) -> Shuffler<rand::rngs::StdRng>` (seedano iz OS entropije).

- [ ] **Step 1: Instaliraj Rust ako ga nema** — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`, zatim `source ~/.cargo/env && cargo --version`. Očekivano: ispisana verzija.
- [ ] **Step 2: Napravi workspace** (`members = ["crates/core", "src-tauri"]`; `src-tauri` dodaj u members tek u Tasku 12, sad samo `crates/core`) i `.gitignore`.
- [ ] **Step 3: Napiši testove koji padaju** u `shuffle.rs`:

```rust
#[test] fn empty_pool_returns_none()            // next() == None
#[test] fn single_track_pool_repeats_forever()  // avoid_first == isti id → ipak ga vraća
#[test] fn no_repeat_within_pass() {            // pool 0..100: prvih 100 next() = permutacija 0..100
}
#[test] fn avoid_first_is_respected() {         // pool 0..10, avoid_first=Some(x), 500 puta s entropy_shuffler: first() != x
}
#[test] fn first_track_distribution_is_uniform() {
    // pool 0..50, 1000 × entropy_shuffler(pool, None).next()
    // svaki id se pojavi ≥1 put; nijedan > 60 puta
}
#[test] fn new_pass_does_not_start_with_last_played() {
    // pool 0..5, 2000 next() poziva; za svaku granicu prolaza (svakih 5) element[i] != element[i-1]
}
#[test] fn remove_skips_removed_track()         // remove(3) → 3 se više ne pojavljuje
```

- [ ] **Step 4:** `cargo test -p smp-core shuffle`. Očekivano: FAIL (compile error).
- [ ] **Step 5: Implementiraj `Shuffler`.** Fisher–Yates radi `rand::seq::SliceRandom::shuffle`. Za avoid: ako je `order[0] == avoid` i `len > 1`, zamijeni `order[0]` s `order[rng.gen_range(1..len)]`. Isto vrijedi pri novom prolazu, gdje je avoid zadnje odsvirani.
- [ ] **Step 6:** `cargo test -p smp-core shuffle`. Očekivano: PASS.
- [ ] **Step 7: Commit** `feat(core): workspace and true-random shuffler`

### Task 2: Žanrovi (split, normalizacija, aliasi)

**Files:**
- Create: `crates/core/src/genre.rs`

**Interfaces:**
- Produces:
  - `pub fn split_genres(raw: &str) -> Vec<String>` dijeli po `;`, `/`, `\0`, ` | `, trima i baca prazne. ID3v1 `(N)` ili samo broj `N` mapira se na ime preko lofty ID3v1 tablice žanrova.
  - `pub fn genre_key(name: &str) -> String`: lowercase, `&`/`'n'`/` n `/`+` → `and`, uklanja sve što nije alfanumeričko, pa primjenjuje ugrađene aliase.
  - `pub const BUILTIN_ALIASES: &[(&str, &str)]` sadrži barem `("dnb","drumandbass")`, `("drumnbass","drumandbass")`, `("hiphop","hiphop")`, `("rnb","randb")`.
  - `pub fn pick_display(spellings: &[(String, usize)]) -> String` vraća najčešći originalni zapis; kod izjednačenja prvi abecedno.

- [ ] **Step 1: Testovi koji padaju:**

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

- [ ] **Step 2:** `cargo test -p smp-core genre`. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj** u `genre.rs`.
- [ ] **Step 4:** `cargo test -p smp-core genre`. Očekivano: PASS.
- [ ] **Step 5: Commit** `feat(core): genre splitting and normalization`

### Task 3: Katalog (SQLite)

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
    - `begin_scan() -> Result<()>`: označi sve `present=0` i resetira `fail_count=0`
    - `mark_seen(rel_path: &str) -> Result<()>`
    - `finish_scan() -> Result<usize>`: briše `present=0` i vraća broj obrisanih
    - `rebuild_genres() -> Result<()>`
    - `genres() -> Result<Vec<GenreCount>>`
    - `playable_tracks(genre_keys: &[String]) -> Result<Vec<TrackId>>`: prazno znači "Sve"; rezultat bez duplikata, bez banned i bez `fail_count >= 3`
    - `track(id: TrackId) -> Result<TrackRow>`
    - `record_failure(id) -> Result<u32>`
    - `record_success(id) -> Result<()>`
    - `set_banned(id, bool) -> Result<()>`
    - `banned() -> Result<Vec<TrackRow>>`
    - `set_measured_lufs(id, f32) -> Result<()>`
    - `add_genre_rule(from_key: &str, to_display: &str) -> Result<()>`: spajanje i preimenovanje
    - `last_first(selection_key: &str) -> Result<Option<TrackId>>`
    - `set_last_first(selection_key: &str, id: TrackId) -> Result<()>`
    - `artists() -> Result<Vec<String>>`
    - `albums(artist: &str) -> Result<Vec<String>>`
    - `album_tracks(artist: &str, album: &str) -> Result<Vec<TrackId>>`: poredano po `track_no`, pa po `rel_path`
  - `pub fn selection_key(genre_keys: &[String]) -> String`: sortirani ključevi spojeni s `|`; prazno → `"*"`.
- Shema: `tracks(id, rel_path UNIQUE, size, mtime, artist, album, title, track_no, duration_ms, raw_genre, rg_track_gain, measured_lufs, fail_count, banned, present)`, `genres(id, key UNIQUE, display)`, `track_genres(track_id, genre_id, PK(track_id, genre_id))` s indeksom na `genre_id`, `genre_rules(from_key PK, to_display)`, `selection_state(selection_key PK, last_first)`. Uključi `PRAGMA journal_mode=WAL`.
- Pravilo za `rebuild_genres`: žanrovi dolaze iz `raw_genre` preko `split_genres` → `genre_key` → `genre_rules` (ako pravilo postoji, ključ postaje `genre_key(to_display)`, a display je `to_display`). Ako pjesma nema žanr, a prva komponenta `rel_path`-a ima `genre_key` jednak nekom postojećem ključu iz tagova, dobiva taj žanr. U suprotnom ide u ključ `"_none"` s displayem `"(bez žanra)"`.

- [ ] **Step 1: Testovi koji padaju:**

```rust
#[test] fn upsert_and_unchanged_detection()          // isti size/mtime → true; drugi mtime → false
#[test] fn multi_genre_track_counts_in_each()        // "Trance; Chillout" → oba imaju count 1
#[test] fn multi_genre_selection_dedups()            // playable_tracks(["trance","chillout"]).len()==1
#[test] fn empty_selection_means_all()
#[test] fn folder_fallback_and_none_bucket()          // "Jazz/x.mp3" bez taga + postoji tag "Jazz" negdje → jazz; "Misc/y.mp3" → _none
#[test] fn three_failures_exclude_until_rescan()     // 3× record_failure → nije u playable; begin_scan+mark_seen+finish_scan → opet je
#[test] fn banned_excluded_and_listed()
#[test] fn genre_rule_merges_and_renames()           // add_genre_rule("chillout","Chill") → jedan žanr "Chill" s oba seta
#[test] fn finish_scan_removes_missing()
#[test] fn selection_key_is_order_independent()      // ["b","a"] == ["a","b"]; [] == "*"
#[test] fn genres_query_fast_on_50k_tracks() {
    // 50 000 upsertova u jednoj transakciji kroz 40 žanrova, rebuild_genres
    // genres() i playable_tracks(&[]) svaki < 200 ms (release i debug)
}
```

- [ ] **Step 2:** `cargo test -p smp-core catalog`. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj `Catalog`.** Ovisnost: `rusqlite` s `bundled`. Dodaj `pub fn transaction<T>(&mut self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T>` za bulk upsert.
- [ ] **Step 4:** `cargo test -p smp-core catalog`. Očekivano: PASS.
- [ ] **Step 5: Commit** `feat(core): sqlite catalog`

### Task 4: MusicSource, LocalSource i tagovi

**Files:**
- Create: `crates/core/src/source/mod.rs`, `crates/core/src/source/local.rs`, `crates/core/src/tags.rs`
- Test fixtures: `crates/core/tests/fixtures/` s po jednom kratkom (1 s, tišina ili ton) `.mp3`, `.flac` i `.ogg` s tagovima (artist, title, genre `"Trance; Chillout"`, a FLAC i s `REPLAYGAIN_TRACK_GAIN=-6.50 dB`) te jednim `broken.mp3` (nasumični bajtovi). Generira se jednom (ffmpeg ako je dostupan, inače skripta `crates/core/tests/fixtures/make.sh`) i commita se.

**Interfaces:**
- Produces:
  - `pub trait ReadSeek: std::io::Read + std::io::Seek + Send {}` s blanket implementacijom
  - `pub struct FileEntry { pub rel_path: String, pub size: u64, pub mtime: i64 }`. `rel_path` uvijek koristi `/` i relativan je na korijen glazbe.
  - `pub trait MusicSource: Send + Sync { fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<()>; fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>>; fn describe(&self) -> String; }`. `walk` vraća samo `.mp3`, `.flac` i `.ogg` (case-insensitive) i preskače skrivene datoteke i mape.
  - `pub struct LocalSource { root: PathBuf }` i `LocalSource::new(root)`
  - `pub fn read_meta(reader: &mut dyn ReadSeek, ext: &str) -> Result<TrackMeta>` (lofty)

- [ ] **Step 1: Testovi koji padaju:**
  - `local_walk_finds_supported_only`: fixtures + `notes.txt` + `.hidden.mp3` → samo 4 audio datoteke.
  - `read_meta_reads_tags`: artist, title i `raw_genre == "Trance; Chillout"` za sva 3 formata, a FLAC ima `rg_track_gain_db == Some(-6.5)`.
  - `read_meta_broken_is_err`
- [ ] **Step 2:** `cargo test -p smp-core source tags`. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj.**
- [ ] **Step 4:** Ponovo pokreni testove. Očekivano: PASS.
- [ ] **Step 5: Commit** `feat(core): music source trait, local source, tag reading`

### Task 5: Skener

**Files:**
- Create: `crates/core/src/scan.rs`

**Interfaces:**
- Consumes: `MusicSource`, `Catalog`, `read_meta`.
- Produces:
  - `pub struct ScanProgress { pub seen: usize, pub read: usize, pub failed: usize, pub current: String }`
  - `pub struct ScanReport { pub total: usize, pub read: usize, pub unchanged: usize, pub failed: usize, pub removed: usize }`
  - `pub fn scan(source: &dyn MusicSource, catalog: &mut Catalog, progress: &mut dyn FnMut(&ScanProgress), cancel: &AtomicBool) -> Result<ScanReport>`
- Ponašanje:
  1. `begin_scan`, pa `walk`.
  2. Za nepromijenjene datoteke samo `mark_seen`. Ostale se otvaraju, prolaze kroz `read_meta` i `upsert`.
  3. Ako `read_meta` padne, datoteka se ipak upsertira s praznim meta (ostaje u katalogu, a pri reprodukciji će se preskočiti) i broji se u `failed`.
  4. Na kraju `finish_scan` i `rebuild_genres`.
  5. Commit u transakcijama po 500 datoteka.
  6. Progress se javlja najviše svakih 100 ms.
  7. Cancel prekida walk i ne poziva `finish_scan`, tako da se ništa ne briše.

- [ ] **Step 1: Testovi koji padaju** (nad `LocalSource` na tempdir kopiji fixtures):
  - `first_scan_reads_all`
  - `second_scan_reads_nothing_unchanged`: `read == 0`, `unchanged == 4`
  - `modified_file_is_reread`
  - `deleted_file_removed`
  - `broken_file_counted_not_fatal`
  - `cancel_keeps_catalog`
- [ ] **Step 2:** Pokreni testove. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj.**
- [ ] **Step 4:** Pokreni testove. Očekivano: PASS.
- [ ] **Step 5: Commit** `feat(core): incremental scanner`

### Task 6: SmbSource (spike pa implementacija)

**Files:**
- Create: `crates/core/src/source/smb.rs`, `crates/core/tests/smb_live.rs`, `.env.test.example`

**Interfaces:**
- Produces:
  - `pub struct SmbConfig { pub host: String, pub share: String, pub root: String, pub username: String, pub password: String, pub domain: Option<String> }`
  - `SmbSource::connect(cfg: SmbConfig) -> Result<Self>` koji implementira `MusicSource`
  - `pub enum SourceError { Auth, Unreachable, NotFound, Io(String), Timeout }` s mapiranjem iz greški biblioteke. UI razlikuje `Auth` od `Unreachable`.

- [ ] **Step 1: Pripremi vjerodajnice.** Zamoli vlasnika da napravi `.env.test` (prema `.env.test.example`) sa SMB korisnikom za `10.111.100.2`. Ne traži lozinku u chatu.
- [ ] **Step 2: Spike (vremenski ograničen, odbacuje se).** Isprobaj pure-Rust crate `smb` (smb-rs). Protiv stvarnog servera mora: (a) izlistati `F/MP3`, (b) pročitati 1 MB s offseta 0 i s offseta u sredini datoteke, (c) vratiti razlikovanu grešku za krivu lozinku. Isto ponovi prema `10.111.0.2` (Samba/Unraid). Ako crate ne prođe neku od točaka na bilo kojem serveru, isprobaj `libsmb2` preko FFI-ja (statički build iz izvora u `build.rs`). Odluku i razlog zapiši u `README.md` sekciju "SMB klijent".
- [ ] **Step 3: Testovi koji padaju** u `tests/smb_live.rs`, svi `#[ignore]` i čitaju `.env.test`:
  - `live_walk_lists_audio`: barem 1 unos, svi `rel_path` bez vodećeg `/`
  - `live_open_read_and_seek`: čitanje prvih 64 KiB, seek na `size/2`, čitanje 64 KiB. Bajtovi se poklapaju s drugim otvaranjem.
  - `live_wrong_password_is_auth_error`
  - `live_scan_reads_tags_sample`: `scan` nad `SmbSource`, prekid nakon 200 datoteka. `read > 0`, `failed < read`.
- [ ] **Step 4:** `cargo test -p smp-core --test smb_live -- --ignored`. Očekivano: FAIL.
- [ ] **Step 5: Implementiraj `SmbSource`.** `open` vraća blokirajući `ReadSeek`. Ako je biblioteka async, koristi interni tokio runtime i `block_on`. Konekcija se dijeli. Ako konekcija padne, napravi jedan reconnect pa vrati grešku. Svaka mrežna operacija ima timeout od 10 s → `SourceError::Timeout`.
- [ ] **Step 6:** Ponovo pokreni testove. Očekivano: PASS na oba servera (`SMB_TEST_HOST` promijeni ručno).
- [ ] **Step 7: Commit** `feat(core): SMB source`

### Task 7: FailoverSource

**Files:**
- Create: `crates/core/src/source/failover.rs`

**Interfaces:**
- Produces: `pub struct FailoverSource { primary: Box<dyn MusicSource>, secondary: Option<Box<dyn MusicSource>> }`, koji implementira `MusicSource`.
  - `walk` ide samo kroz primary. Ako primary vrati `Unreachable`/`Timeout`, a secondary postoji, ide kroz secondary.
  - `open` pokušava primary, a nakon `Unreachable`/`Timeout`/`NotFound` pokušava secondary s istim `rel_path`.
  - `pub fn active(&self) -> Which` (`Primary`/`Secondary`) se koristi za status u UI-ju.

- [ ] **Step 1: Testovi koji padaju** s lažnim izvorom (`FakeSource` u testu: konfigurabilna greška po putanji):
  - `open_falls_back_on_unreachable`
  - `open_falls_back_on_not_found`
  - `auth_error_does_not_fall_back`
  - `walk_uses_secondary_when_primary_down`
  - `no_secondary_returns_primary_error`
- [ ] **Step 2:** Pokreni testove. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj.**
- [ ] **Step 4:** Pokreni testove. Očekivano: PASS.
- [ ] **Step 5: Ručna provjera** (pomoću `smb_live` testa ili ad-hoc primjera): ista `rel_path` uzorka od 20 datoteka postoji na oba servera. Ako ne postoji, zapiši to u README i javi vlasniku.
- [ ] **Step 6: Commit** `feat(core): primary/secondary failover source`

### Task 8: PrefetchReader

**Files:**
- Create: `crates/core/src/audio/prefetch.rs`

**Interfaces:**
- Produces: `pub struct PrefetchReader`, koji implementira `Read + Seek + Send`.
  - `PrefetchReader::new(inner: Box<dyn ReadSeek>, len: u64, cfg: PrefetchConfig) -> Self`
  - `pub struct PrefetchConfig { pub chunk: usize /*256 KiB*/, pub ahead: usize /*8 MiB*/, pub read_timeout: Duration /*10 s*/ }` s `Default`.
- Ponašanje:
  - Pozadinska dretva čita unaprijed do `ahead` bajtova.
  - `read` blokira dok podaci ne stignu ili dok ne istekne `read_timeout`. Nakon isteka vraća `io::ErrorKind::TimedOut`.
  - Seek unutar buffera ne zove inner. Seek izvan buffera poništava buffer i premješta dretvu.
  - `Drop` zaustavlja dretvu.

- [ ] **Step 1: Testovi koji padaju** (inner je `Cursor` od 5 MiB nasumičnih bajtova, uz `SlowReader` wrapper):
  - `reads_match_source`: cijelo čitanje = izvor
  - `seek_forward_backward_match_source`
  - `prefetches_ahead`: nakon 50 ms bez čitanja dretva je pročitala ≥ 1 MiB (provjeri preko brojača u `SlowReader`)
  - `read_times_out_when_inner_stalls`: inner blokira zauvijek, `read_timeout` je 200 ms → `TimedOut` unutar 400 ms
  - `inner_error_propagates`
- [ ] **Step 2:** Pokreni testove. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj.**
- [ ] **Step 4:** Pokreni testove. Očekivano: PASS.
- [ ] **Step 5: Commit** `feat(core): prefetching reader`

### Task 9: Dekoder

**Files:**
- Create: `crates/core/src/audio/decoder.rs`

**Interfaces:**
- Produces:
  - `pub struct Decoder`
  - `Decoder::open(reader: Box<dyn ReadSeek>, len: Option<u64>, ext: &str, out_rate: u32) -> Result<Self>`
  - `fn read(&mut self, out: &mut [f32]) -> Result<usize>`: interleavani stereo f32 na `out_rate`. `Ok(0)` znači kraj.
  - `fn duration_ms(&self) -> Option<u64>`
  - Mono se duplicira u L/R. Za više od 2 kanala koristi downmix L = FL + 0,707·C (+ SL), R analogno, pa normaliziraj da ne klipa.
  - Ako je izvorni rate ≠ `out_rate`, resampla rubato (`FftFixedIn` ili `SincFixedIn`).
- Symphonia: features `mp3`, `flac`, `ogg`, `vorbis`. `MediaSourceStream` nad wrapperom koji implementira `MediaSource` (`is_seekable=true`, `byte_len=len`). Dekoderske greške unutar paketa (`DecodeError`) se preskaču. `IoError` i neočekivani EOF prije prvog uzorka su `Err`.

- [ ] **Step 1: Testovi koji padaju** (fixtures iz Taska 4, svaka 1 s ton na 44,1 kHz):
  - `decodes_mp3_flac_ogg`: `out_rate=48000` → ukupno ≈ 48000 ± 2400 frameova po datoteci, RMS > 0,01
  - `broken_file_open_fails`
  - `mono_is_duplicated`: L == R
- [ ] **Step 2:** Pokreni testove. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj.**
- [ ] **Step 4:** Pokreni testove. Očekivano: PASS.
- [ ] **Step 5: Commit** `feat(core): symphonia decoder with resampling`

### Task 10: DSP (crossfade, gain, ceiling, loudness)

**Files:**
- Create: `crates/core/src/audio/dsp.rs`

**Interfaces:**
- Produces:
  - `pub fn equal_power(t: f32) -> (f32, f32)`: vraća `(out_gain, in_gain) = (cos(t·π/2), sin(t·π/2))`, `t` se clampa na [0,1]
  - `pub fn track_gain_db(rg_track_gain_db: Option<f32>, measured_lufs: Option<f32>) -> f32`: RG ako postoji, inače `-18 - lufs`, inače 0. Rezultat se clampa na [-15, +6].
  - `pub fn db_to_lin(db: f32) -> f32`
  - `pub fn apply_ceiling(buf: &mut [f32])`: soft-knee limiter na -1 dBFS (0,891). Ispod 0,8 ne mijenja signal, iznad komprimira i nikad ne prelazi 0,891.
  - `pub struct LoudnessMeter`: `new(rate)`, `push(&[f32])`, `finish(self) -> Option<f32>` (integrated LUFS, `None` ako je izmjereno < 30 s). Koristi crate `ebur128`.

- [ ] **Step 1: Testovi koji padaju:**

```rust
#[test] fn equal_power_endpoints_and_constant_power() {
    // t=0 → (1,0); t=1 → (0,1); za t∈{0.25,0.5,0.75}: o²+i² ≈ 1 (±1e-4)
}
#[test] fn gain_prefers_replaygain_then_lufs_then_zero() {
    // (Some(-6.5),Some(-10.0)) → -6.5; (None,Some(-10.0)) → -8.0; (None,None) → 0.0
    // (None,Some(-40.0)) → 6.0 (clamp); (Some(-30.0),None) → -15.0
}
#[test] fn ceiling_never_exceeds_and_is_transparent_below_knee() {
    // ulaz 0.5 ostaje 0.5; ulazi 1.0, 2.0, -3.0 → |y| ≤ 0.891
}
#[test] fn loudness_of_known_tone() {
    // 997 Hz sinus amplitude 0.1 stereo, 35 s, 48 kHz → LUFS ≈ -20.0 ± 1.0
    // 10 s ulaza → finish() == None
}
```

- [ ] **Step 2:** Pokreni testove. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj.**
- [ ] **Step 4:** Pokreni testove. Očekivano: PASS.
- [ ] **Step 5: Commit** `feat(core): dsp helpers`

### Task 11: Izlaz i Player engine

**Files:**
- Create: `crates/core/src/audio/output.rs`, `crates/core/src/audio/player.rs`, `crates/core/src/audio/mod.rs`

**Interfaces:**
- Consumes: `MusicSource`, `PrefetchReader`, `Decoder`, `dsp::*`, `Shuffler`, `TrackId`.
- Produces (`output.rs`):
  - `pub fn list_output_devices() -> Vec<String>`
  - `pub fn default_output_device() -> Option<String>`
  - `pub trait Sink: Send { fn sample_rate(&self) -> u32; fn write(&mut self, stereo: &[f32]) -> SinkWrite; }` gdje je `pub enum SinkWrite { Ok(usize) /*koliko je stalo*/, DeviceLost }`
  - `pub struct CpalSink` i `CpalSink::open(device: Option<&str>) -> Result<(Self, String /*stvarno ime*/)>`: rtrb ring od ~200 ms između engine dretve i cpal callbacka. Stereo se mapira na kanale uređaja (višak kanala dobiva tišinu). Callback na grešku postavlja zastavicu, pa sljedeći `write` vraća `DeviceLost`.
  - `pub trait SinkFactory: Send { fn open(&mut self, device: Option<&str>) -> Result<(Box<dyn Sink>, String)>; }` s implementacijom za cpal.
- Produces (`player.rs`):
  - `pub trait TrackProvider: Send { fn next_track(&mut self) -> Option<PlayItem>; fn on_failed(&mut self, id: TrackId); fn on_played(&mut self, id: TrackId, measured_lufs: Option<f32>); }`
  - `pub struct PlayItem { pub id: TrackId, pub rel_path: String, pub gain_db: f32 }`
  - `pub enum Command { Play(Box<dyn TrackProvider>), Pause, Resume, TogglePause, Next, SetVolume(f32 /*0..1*/), SetDevice(Option<String>), SetCrossfade { enabled: bool, secs: u8 }, SetNormalize(bool), Stop }`
  - `pub enum Event { TrackStarted { id: TrackId }, TrackSkipped { id: TrackId, reason: String }, Paused(bool), DeviceChanged(String), Idle }`
  - `pub struct Player` i `Player::spawn(source: Arc<dyn MusicSource>, sinks: Box<dyn SinkFactory>, events: Sender<Event>) -> Self`, uz `fn send(&self, c: Command)`.
- Ponašanje engine dretve:
  - Glasnoća se primjenjuje kao linearni gain s rampom od 20 ms.
  - Normalizacija množi s `db_to_lin(gain_db)` ako je uključena, nakon toga ide `apply_ceiling`.
  - Pjesma se otvara kao `source.open` → `PrefetchReader` → `Decoder`. Otvaranje sljedeće pjesme kreće kad je do kraja trenutne ostalo `crossfade_secs + 10 s` (ili 10 s kad je crossfade isključen).
  - Kad do kraja ostane `crossfade_secs`, kreće crossfade (`equal_power`). Bez crossfadea pjesme idu jedna za drugom, bez umetnute tišine.
  - `Next` crossfadea prema sljedećoj u 0,7 s. Ako sljedeća nije spremna, ide fade-out od 0,3 s pa start sljedeće. `Next` dok fade već traje odmah završi trenutni fade (stara pjesma se izbacuje) i počinje novi prema sljedećoj, tako da istovremeno nikad ne sviraju više od dvije pjesme.
  - Greška pri otvaranju, `TimedOut` ili greška dekodiranja prije prvog uzorka: `on_failed` + `TrackSkipped` pa odmah sljedeća pjesma. Ako se dogodi 20 neuspjeha zaredom, emitira se `Idle` i engine čeka.
  - Kad pjesma uredno završi: `on_played` s LUFS iz `LoudnessMeter` (mjerenje se radi prije gaina).
  - `SinkWrite::DeviceLost`: otvara se zadani uređaj (`open(None)`) i emitira `DeviceChanged`.
  - Dok je pauzirano, engine ne piše u sink.

- [ ] **Step 1: Testovi koji padaju** (`FakeSink` bilježi uzorke na 48 kHz i ima konfigurabilni `DeviceLost`, `FakeProvider` vraća zadane stavke, a izvor je `LocalSource` nad fixtures i generiranim tonovima od 20 s, od kojih svaki ima drugu frekvenciju za prepoznavanje):
  - `plays_tracks_in_provider_order`
  - `broken_track_is_skipped_and_reported`: `TrackSkipped` za `broken.mp3`, a sljedeća pjesma svira
  - `stalled_source_skips_track`: `open` vraća reader koji blokira, `read_timeout` se u testu skraćuje na 200 ms → `TrackSkipped`
  - `crossfade_overlaps_for_configured_secs`: crossfade 2 s → u izlazu postoji ~2 s ± 0,1 s gdje su prisutne obje frekvencije (Goertzel)
  - `no_crossfade_means_no_gap`: nema više od 50 ms tišine na spoju
  - `rapid_next_skips_once_per_press`: 5× `Next` u 50 ms → 5 `TrackStarted` eventova (svaki za drugu stavku), u izlazu nikad više od 2 frekvencije odjednom, bez panike
  - `pause_stops_output`
  - `device_error_falls_back_to_default`: FakeSink vrati `DeviceLost` → factory dobije `open(None)` i dolazi `DeviceChanged`
  - `normalization_applies_gain`: `gain_db = -6` → RMS ≈ pola (±10 %). Kad je `SetNormalize(false)`, RMS je nepromijenjen.
- [ ] **Step 2:** `cargo test -p smp-core player`. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj `output.rs` i `player.rs`.**
- [ ] **Step 4:** Ponovo pokreni testove. Očekivano: PASS.
- [ ] **Step 5: Ručni dimni test.** Primjer `crates/core/examples/play_local.rs <dir>` pusti 3 pjesme kroz `CpalSink` sa crossfadeom od 5 s. Prijelaz se mora čuti.
- [ ] **Step 6: Commit** `feat(core): audio output and player engine`

### Task 12: Tauri ljuska, postavke, vjerodajnice, stanje i komande

**Files:**
- Create: `src-tauri/*` (`cargo tauri init` ili ručno), `src-tauri/src/{main,settings,credentials,state,commands}.rs`, privremeni `ui/index.html`
- Modify: `Cargo.toml` (dodaj `src-tauri` u members)

**Interfaces:**
- Consumes: sve iz `smp-core`.
- Produces:
  - `pub struct ServerConfig { pub host: String, pub share: String, pub root: String, pub username: String, pub domain: Option<String> }`
  - `pub struct Settings { pub primary: ServerConfig, pub secondary: Option<ServerConfig>, pub volume: f32, pub device: Option<String>, pub crossfade_enabled: bool, pub crossfade_secs: u8, pub normalize: bool, pub notify: bool, pub autostart: bool, pub last_selection: Vec<String> }` (serde). Defaulti su iz Global Constraints, a `volume` je 0,7. Sprema se u `app_config_dir()/settings.json`. `crossfade_secs` se pri učitavanju clampa na 1..=10.
  - `credentials::{get(host,user) -> Option<String>, set(host,user,pw) -> Result<()>}` preko `keyring`.
  - `AppState`: `Mutex<Catalog>`, `Player`, `Arc<FailoverSource>` (može se rekreirati), status veze, `CatalogProvider` (`TrackProvider` nad `Catalog` + `Shuffler`, ili nad albumom redom).
  - Komande (snake_case, sve vraćaju `Result<_, String>`): `get_settings`, `save_settings(settings, primary_password: Option<String>, secondary_password: Option<String>)`, `test_connection(which: "primary"|"secondary") -> "ok"|"auth"|"unreachable"`, `start_scan`, `cancel_scan`, `get_genres -> Vec<GenreCount>`, `play_genres(keys: Vec<String>)`, `play_album(artist, album)`, `toggle_pause`, `next`, `set_volume(v)`, `list_devices`, `set_device(name: Option<String>)`, `set_crossfade(enabled, secs)`, `set_normalize(b)`, `ban_current`, `list_banned`, `unban(id)`, `merge_genre(from_key, into_display)`, `list_artists`, `list_albums(artist)`, `now_playing -> Option<NowPlaying{id, artist, title, album, genre_display, paused}>`.
  - Eventovi prema UI-ju: `now-playing`, `scan-progress`, `scan-done`, `connection-status`, `track-skipped`.
- Ponašanje:
  - `play_genres` gradi `entropy_shuffler(catalog.playable_tracks(keys), catalog.last_first(selection_key(keys)))` i pri prvom `next_track` sprema `set_last_first`.
  - `CatalogProvider::on_failed` zove `record_failure` i `shuffler.remove`.
  - Pri startu: ako je katalog prazan, a postavke postoje, automatski pokreće skeniranje. Inače ne skenira.
  - Skeniranje radi u zasebnoj dretvi s vlastitom `Catalog` konekcijom (WAL).
  - Status veze: ako izvor nije dostupan, katalog i UI rade iz cachea. Pozadinski se svakih 15 s pokušava ponovno spojiti, uz event `connection-status` (`ok` / `secondary` / `unreachable` / `auth`). Tekst za `unreachable`: "Disk nije dostupan. Je li uključen WireGuard/VPN?"
  - Test: `connection_status_maps_errors`, gdje se `Unreachable` → `unreachable`, `Auth` → `auth`, a fallback na secondary → `secondary`.

- [ ] **Step 1: Testovi koji padaju** (u `src-tauri`, čista logika bez Tauri runtimea):
  - `settings_defaults_match_spec`: host `10.111.100.2`, share `F`, root `MP3`, crossfade uključen i 5, normalize true, notify false, autostart false
  - `settings_clamp_crossfade`: 0 → 1, 15 → 10
  - `settings_never_serialize_password`: JSON ne sadrži `password`
  - `catalog_provider_records_last_first_and_failures`
- [ ] **Step 2:** `cargo test -p smb-music-player`. Očekivano: FAIL.
- [ ] **Step 3: Implementiraj.** `tauri.conf.json`: `withGlobalTauri: true`, `frontendDist: "../ui"`, jedan prozor `main` 420×640 koji se može resizati, identifier `hr.pvt.smb-music-player`.
- [ ] **Step 4:** Ponovo pokreni testove. Očekivano: PASS. Zatim `cargo tauri dev`: prozor se otvara i u konzoli DevToolsa `__TAURI__.core.invoke('get_settings')` vraća defaulte.
- [ ] **Step 5: Commit** `feat(app): tauri shell, settings, keychain, commands`

### Task 13: UI

**Files:**
- Create/Modify: `ui/index.html`, `ui/style.css`, `ui/app.js`

**Interfaces:**
- Consumes: komande i eventovi iz Taska 12.
- Sadržaj:
  - **Gornja traka** "sada svira": izvođač – naslov, žanr, ⏯, ⏭, 🚫 (ban s potvrdom u 1 klik) i slider glasnoće.
  - **Kartica Žanrovi**: lista s brojem pjesama. Klik na ime odmah pušta taj žanr, checkboxovi + gumb "Pusti odabrano" puštaju više žanrova, a gumb "Sve" pušta sve. Polje za filtriranje. Desni klik ili ⋯ na žanru nudi "Spoji u…/Preimenuj".
  - **Kartica Izvođači**: pretraga, lista (renderira najviše 300 rezultata uz poruku "suzi pretragu"), klik → albumi, klik na album → `play_album`.
  - **Kartica Postavke**:
    - glavni i rezervni server (host, share, putanja, korisnik, lozinka, domena) s gumbom "Testiraj vezu" za svaki
    - padajući izbornik izlaznog uređaja
    - crossfade (uključi + slider 1–10 s)
    - normalizacija, obavijesti, pokretanje sa sustavom
    - "Osvježi katalog" s progresom i prekidom
    - lista zabranjenih pjesama s "Vrati"
  - Status veze: zelena/žuta (rezervni)/crvena točka uz tekst.
  - Prvo pokretanje bez postavki otvara karticu Postavke.
  - Stil: kompaktan, sistemski font, svijetla/tamna tema preko `prefers-color-scheme`.

- [ ] **Step 1: Implementiraj.**
- [ ] **Step 2: Ručna provjera** (`cargo tauri dev`, stvarni server):
  1. Unesi postavke i testiraj vezu (ok). Kriva lozinka mora javiti "Pogrešno korisničko ime ili lozinka".
  2. Pokreni skeniranje: progres raste, a nakon kraja se pojave žanrovi s brojevima.
  3. Klikni žanr: svira za manje od 3 s.
  4. Pet ponovnih pokretanja istog žanra daje pet različitih prvih pjesama.
  5. ⏭ prelazi uz kratki fade.
  6. Crossfade od 10 s se čuje na prirodnom kraju pjesme.
  7. Promjena uređaja prebacuje zvuk.
  8. Ban radi i pjesma se pojavljuje u postavkama.
  9. Restart aplikacije ne pokreće skeniranje.
- [ ] **Step 3: Commit** `feat(ui): genres, artists, settings views`

### Task 14: Tray, medijske tipke, obavijesti, autostart, ponašanje prozora

**Files:**
- Create: `src-tauri/src/tray.rs`, `src-tauri/src/media.rs`
- Modify: `src-tauri/src/main.rs`, `src-tauri/Cargo.toml`, `src-tauri/icons/tray-next.png` (template ikona ⏭, 22 pt @1x/@2x)

**Interfaces:**
- Consumes: `AppState`, komande.
- Ponašanje:
  - **Tray:** `TrayIconBuilder` s ikonom ⏭ kao template slikom (macOS) i `show_menu_on_left_click(false)`.
    - Lijevi klik (`TrayIconEvent::Click { button: Left, button_state: Up }`) radi `next`.
    - Desni klik otvara izbornik:
      - stavka "Izvođač – Naslov" (disabled, ažurira se na `now-playing`)
      - "Pauza"/"Nastavi" (tekst se mijenja)
      - "Sljedeća"
      - "Nikad više ovu pjesmu"
      - podizbornik "Žanr": "Sve" + 30 najvećih žanrova po broju pjesama + ostali žanrovi iz `last_selection` ako nisu među njima
      - "Prikaži prozor"
      - "Izlaz"
    - Tooltip prikazuje trenutnu pjesmu.
  - **Prozor:** `CloseRequested` → `prevent_close` + `hide`. Na macOS-u `ActivationPolicy::Accessory`, dakle bez ikone u Docku.
  - **Medijske tipke i Now Playing:** `souvlaki::MediaControls`. Play/Pause/Toggle/Next mapiraju se na komande, a metadata se ažurira na `now-playing`.
  - **Obavijest** (`tauri-plugin-notification`) pri `TrackStarted`, samo ako je `notify` true.
  - **Autostart** (`tauri-plugin-autostart`, LaunchAgent) prati postavku `autostart`.
  - **Jedna instanca:** `tauri-plugin-single-instance`. Drugo pokretanje samo prikaže prozor.

- [ ] **Step 1: Implementiraj.**
- [ ] **Step 2: Ručna provjera:**
  1. Zatvori prozor: glazba svira dalje, ikona je u menu baru i nema je u Docku.
  2. Jedan lijevi klik na ikonu preskače pjesmu.
  3. Desni klik prikazuje izbornik s trenutnom pjesmom. Promjena žanra iz izbornika radi.
  4. F8 (play/pause) i F9 (next) rade, a pjesma se vidi u Control Centeru.
  5. Obavijest se pojavljuje samo kad je uključena.
  6. Autostart uključen → nakon odjave i prijave aplikacija je u trayu.
  7. Ako lijevi klik na macOS-u ne stiže pouzdano kao `Click`, prebaci "Sljedeća" na prvu stavku izbornika i zapiši to u README (spec §2.12).
- [ ] **Step 3: Commit** `feat(app): tray, media keys, notifications, autostart`

### Task 15: Pakiranje, CI i README

**Files:**
- Create: `.github/workflows/build.yml`, `README.md` (ili dopuna)
- Modify: `src-tauri/tauri.conf.json` (bundle targets `dmg` i `app`, a za Windows `msi` i `nsis`)

- [ ] **Step 1:** `cargo tauri build --target universal-apple-darwin` (prethodno `rustup target add aarch64-apple-darwin x86_64-apple-darwin`). Očekivano: `.dmg` u `target/universal-apple-darwin/release/bundle/dmg/`.
- [ ] **Step 2: Ručna provjera.** Instaliraj iz `.dmg`, pokreni (README opisuje `xattr -dr com.apple.quarantine "/Applications/SMB Music Player.app"` za nepotpisanu aplikaciju) i ponovi checkliste iz Taskova 13 i 14 u kratkom obliku.
- [ ] **Step 3: Workflow.** `build.yml`: matrix `macos-latest` i `windows-latest`, `dtolnay/rust-toolchain@stable`, `cargo test -p smp-core` (bez ignored testova), `tauri-apps/tauri-action` builda, a artefakti (`.dmg`, `.msi`/`.exe`) se uploadaju. Okida se na push na `main` i na `workflow_dispatch`.
- [ ] **Step 4: README.** Opisuje: što aplikacija radi, instalaciju, prvo postavljanje, SMB klijent (odluka iz Taska 6), poznata ograničenja (nepotpisano, gapless nije sample-točan) i kako pokrenuti live testove.
- [ ] **Step 5: Commit** `chore: packaging, CI workflow, README`
- [ ] **Step 6: Predaja.** Push na GitHub je vanjska radnja. Pitaj vlasnika za repozitorij (privatni) prije pusha. Windows build se smatra uspjelim tek kad CI prođe i vlasnik isproba instalaciju.
