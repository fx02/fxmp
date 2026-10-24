# SMB music player

macOS-first Tauri 2 player koji pušta glazbu izravno s SMB dijeljenja
(MP3, FLAC, OGG Vorbis). Dizajn: `docs/superpowers/specs/2026-10-24-smb-music-player-design.md`.

## Što aplikacija radi

- Spaja se sama na SMB server (ne treba OS mount), skenira mapu s glazbom u
  lokalni katalog i grupira pjesme po žanrovima iz tagova.
- Pušta nasumično (shuffle) iz odabranih žanrova: bez ponavljanja unutar
  prolaza, svako pokretanje odabira počinje drugom pjesmom.
- Crossfade (uključen, 5 s, podesivo 1–10 s), ručni "sljedeća" radi kratki fade
  od 0,7 s, ujednačavanje glasnoće (ReplayGain ili izmjereno, cilj -18 LUFS).
- Živi u traci izbornika (bez ikone u Docku), medijske tipke i Now Playing,
  neobavezne obavijesti i pokretanje pri prijavi.
- Opcionalni rezervni server: ako glavni ne odgovara, koristi se rezervni.

## Instalacija (macOS 12+, Apple Silicon i Intel)

1. Preuzmi `.dmg` (`SMB Music Player_0.1.0_universal.dmg`), otvori ga i
   povuci aplikaciju u `Applications`.
2. Aplikacija nije potpisana ni notarizirana, pa je macOS blokira pri prvom
   pokretanju. Ukloni oznaku karantene:

   ```sh
   xattr -dr com.apple.quarantine "/Applications/SMB Music Player.app"
   ```

3. Pokreni aplikaciju. Ikona se pojavljuje u traci izbornika.

Build iz izvora: `cargo tauri build --target universal-apple-darwin`
(`rustup target add aarch64-apple-darwin x86_64-apple-darwin`); `.dmg` je u
`target/universal-apple-darwin/release/bundle/dmg/`. Verzija Rusta je
pripeta u `rust-toolchain.toml`. Ikone se generiraju skriptom
`src-tauri/icons/gen_app_icon.py` + `cargo tauri icon`.

## Prvo postavljanje

Pri prvom pokretanju otvara se prozor. U **Postavke** upiši:

- **Server**, **Share**, **Putanja** (defaulti: `10.111.100.2`, `F`, `MP3`),
- **Korisnik** i **Lozinka** (domena je opcionalna),
- po želji uključi **Rezervni server** (predloženo `10.111.0.2`) i upiši njegove podatke.

Klikni "Testiraj vezu", zatim "Spremi" i "Osvježi katalog". Lozinke se čuvaju
samo u macOS keychainu (servis `smb-music-player`), nikad u datoteci ni u logu.
macOS će pitati smije li aplikacija čitati keychain: odaberi **Uvijek dopusti**.

## Korištenje

- Kartica **Žanrovi**: klik na žanr pušta samo taj žanr. Kvačicama možeš odabrati
  više žanrova pa "Pusti odabrano (n)". **Sve** pušta cijelu knjižnicu.
  Desni klik (ili ⋯) na žanr: "Spoji u…" i "Preimenuj…".
- Tipke u prozoru: ⏯ pauza, ⏭ sljedeća, 🚫 "nikad više ne puštaj ovu pjesmu"
  (traži potvrdu, drugi klik unutar 3 s). Zabranjene pjesme se vraćaju u
  Postavkama gumbom **Vrati**.
- Traka izbornika: **lijevi klik na ikonu = sljedeća pjesma**, desni klik otvara
  izbornik (trenutna pjesma, pauza, žanr, zabrana, prikaži prozor, izlaz).
- **F8** je pauza/nastavi, **F9** sljedeća (i Control Center).
- Zatvaranje prozora ne gasi aplikaciju; izlaz je iz izbornika u traci.

## Poznata ograničenja

- Aplikacija nije potpisana (vidi `xattr` gore).
- Gapless nije sample-točan (prijelaz između pjesama nije na uzorak točno).
- Izlistavanje mape na serveru traje 15–40 s i bez prikaza napretka.
- Prvo puno skeniranje (~59 000 datoteka) traje otprilike 25–75 min, ali se
  žanrovi pojavljuju već tijekom skeniranja.
- Ako se pjesma pusti prije nego što odgovoriš na keychain upit, pjesme se
  preskaču dok se lozinka ne dobije.
- Windows: CI gradi `.msi`/`.exe`, ali medijske tipke (souvlaki traži HWND) su
  za sada samo na macOS-u, a build nije isprobavan na pravom Windowsu.

## Live testovi i testiranje

```sh
cargo test --workspace            # bez live testova
node --test ui/*.test.mjs
cp .env.test.example .env.test    # upiši podatke; datoteka je u .gitignore
cargo test -p smp-core --test smb_live -- --ignored --test-threads=1
```

Live testovi (`cargo test -- --ignored`) samo čitaju sa servera, ništa se ne piše.
CI (`.github/workflows/build.yml`, push na `main` i ručno) vrti testove i gradi
macOS `.dmg` i Windows `.msi`/`.exe` kao artefakte.

## Checklista za vlasnika (ručne provjere)

Ovo se ne može provjeriti automatski (treba slušati ili kliknuti):

- [ ] Crossfade je čujan i bez klika za 1, 5 i 10 s (prirodni kraj pjesme).
- [ ] ⏭ radi kratki fade od 0,7 s, bez klika.
- [ ] Gapless/album: prijelaz pjesama u albumu je uredan.
- [ ] Promjena izlaznog uređaja prebacuje zvuk; vađenje slušalica ne ruši zvuk.
- [ ] Ujednačavanje glasnoće: pjesme su približno jednako glasne (uključeno/isključeno).
- [ ] Pet pokretanja istog žanra daje pet različitih prvih pjesama.
- [ ] Izgled u svijetlom i tamnom načinu (420×640, dugi naslovi, chip žanra, ⋯).
- [ ] Lijevi klik na ikonu u traci pouzdano preskače pjesmu (ako Click ne stiže
      pouzdano: premjestiti "Sljedeća" na prvu stavku izbornika).
- [ ] Desni klik: izbornik, naziv pjesme se osvježava, izbor žanra pušta taj žanr.
- [ ] F8/F9 rade i pjesma se vidi u Control Centeru.
- [ ] Obavijesti se pojavljuju samo kad su uključene (isprobati s instaliranim .app).
- [ ] Pokretanje pri prijavi: nakon odjave/prijave aplikacija je u traci, prozor skriven.
- [ ] Nema ikone u Docku; "Prikaži prozor" i drugo pokretanje dižu prozor naprijed.
- [ ] Puno prvo skeniranje do kraja, uz postupno pojavljivanje žanrova.
- [ ] Keychain upit ("Uvijek dopusti") i da nakon toga pjesme normalno sviraju.
- [ ] Prelazak na rezervni server kad glavni nije dostupan.
- [ ] Žanr chip, zabrana i "Vrati" ponašaju se ispravno na pravom slušanju.

## SMB klijent

**Odluka:** pure-Rust crate [`smb`](https://crates.io/crates/smb) (smb-rs)
`=0.12.1`, u **async** načinu, pokretan iz privatnog tokio runtimea
(`crates/core/src/source/smb.rs`). `libsmb2` preko FFI-ja nije bio potreban.

**Spike (24. 10. 2026)** protiv oba stvarna servera (Windows 10 `10.111.100.2`
i Unraid/Samba `10.111.0.2`, share `F`, putanja `MP3`):

| Kriterij | Windows 10 | Samba |
|---|---|---|
| izlistavanje `F/MP3` | da | da |
| 1 MB s offseta 0 i iz sredine datoteke | da | da (vidi niže) |
| kriva lozinka → zasebna greška (`Logon Failure 0xC000006D`) | da | da |

Usput pronađena tri problema u crateu i kako su riješena:

1. **Sinkroni (`multi_threaded`) način gubi podatke.** Prijemna petlja čita
   socket s timeoutom od 100 ms i `read_exact`; kad veliki READ odgovor stiže
   u dijelovima sporije od toga, pročitani bajtovi se bace, tok se raspadne
   ("bad magic") i zahtjev istekne nakon 10 s. Async (tokio) backend nema taj
   problem, pa se koristi on, a `MusicSource` ostaje blokirajući preko
   `block_on`. Posljedica: metode `SmbSource` se ne smiju zvati iz async
   konteksta (koristiti `spawn_blocking`).
2. **Nema `TCP_NODELAY`.** Svaki zahtjev se šalje u nekoliko malih pisanja pa
   Nagle + odgođeni ACK dodaju ~40–80 ms po zahtjevu. `smb-transport` 0.12.1 je
   zato vendoran u `vendor/smb-transport` s jednom dodanom linijom
   (`set_nodelay(true)`), vidi `vendor/smb-transport/SMP-PATCH.md`. Obilazak
   ~10 900 datoteka: 255 s bez zakrpe, 70 s sa zakrpom.
3. **Izlistavanje mape povremeno visi do timeouta.** `Directory::query` stream
   u `smb` 0.12.1 gubi buđenje (`notify_waiters` umjesto `notify_one` u
   `poll_next`), pa ne pošalje završni upit i čeka do timeouta, nakon čega
   slijedi ponovno spajanje i ponovni pokušaj. Na Sambi (`10.111.0.2`) puni
   obilazak (~59 400 datoteka) zbog toga je završavao greškom `Timeout`.
   `smb` 0.12.1 je zato vendoran u `vendor/smb` s tom jednom promjenom, vidi
   `vendor/smb/SMP-PATCH.md`. Puni obilazak Sambe: `Timeout` nakon 130 s bez
   zakrpe, ~38 s bez greške sa zakrpom.

Ostalo:
- Čitanje ide u komadima od 256 KiB (read-ahead buffer po otvorenoj datoteci),
  nikad se ne skida cijela datoteka.
- Svaka mrežna operacija ima timeout 10 s (`SourceError::Timeout`), osim
  izlistavanja mape, koje ima 15 s. Nakon
  prekinute veze ili timeouta veza se jednom ponovno uspostavi (npr. poluotvorena
  veza nakon spavanja), a tek onda se vraća greška.
- Podmapa koja se ne može izlistati zbog vlastitog razloga (pristup odbijen,
  nestala, I/O greška) se preskače uz log. Prekid veze ili timeout prekida cijeli
  obilazak greškom, inače bi skener obrisao pjesme iz neobiđenih mapa. Greška je
  i kad se ne može izlistati korijen.
- Obilazak izlistava do 8 mapa istodobno na istoj vezi (svaka na svojoj
  dretvi), pa redoslijed pronađenih datoteka nije određen. Pravila za greške
  iznad vrijede jednako.
- Skener (`scan`) otvara i čita tagove novih/promijenjenih datoteka u 8
  paralelnih dretvi (`ScanOptions { workers }`); u katalog piše samo pozivajuća
  dretva, u transakcijama po 500. Mjereno na `10.111.100.2`, prvo skeniranje
  500 datoteka: 28,0 s s jednom dretvom, 9,8 s s osam (2,8×).
  Tijekom skeniranja žanrovi se ponovno grade nakon svakih 5000 novih
  datoteka, pa je knjižnica upotrebljiva i prije kraja prvog skeniranja.

Live testovi (samo čitanje, ništa se ne piše na servere):

```sh
cp .env.test.example .env.test   # upiši podatke; datoteka je u .gitignore
cargo test -p smp-core --test smb_live -- --ignored --test-threads=1
```

Testovi idu na glavni server i, ako je `SMB_TEST2_HOST` postavljen, i na rezervni.
