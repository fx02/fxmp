# SMB music player

macOS-first Tauri 2 player that streams music straight from an SMB share.
Design: `docs/superpowers/specs/2026-10-24-smb-music-player-design.md`.

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

Usput pronađena dva problema u crateu i kako su riješena:

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

Ostalo:
- Čitanje ide u komadima od 256 KiB (read-ahead buffer po otvorenoj datoteci),
  nikad se ne skida cijela datoteka.
- Svaka mrežna operacija ima timeout 10 s (`SourceError::Timeout`), osim
  izlistavanja mape, koje ima 30 s. Nakon
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
  500 datoteka: 30,5 s s jednom dretvom, 12,7 s s osam (2,4×).
  Tijekom skeniranja žanrovi se ponovno grade nakon svakih 5000 novih
  datoteka, pa je knjižnica upotrebljiva i prije kraja prvog skeniranja.
- **Poznat problem u crateu (nije zakrpan):** `Directory::query` stream u
  `smb` 0.12.1 ponekad izgubi buđenje (`notify_waiters` umjesto
  `notify_one` u `poll_next`) i čeka do timeouta, nakon čega slijedi
  ponovno spajanje i ponovni pokušaj. Izlistavanje je zato povremeno sporo, a
  na Sambi (`10.111.0.2`) puni obilazak zna završiti greškom `Timeout`.
  Provjereno: s tom jednom promjenom puni obilazak Sambe traje ~38 s bez
  greške.

Live testovi (samo čitanje, ništa se ne piše na servere):

```sh
cp .env.test.example .env.test   # upiši podatke; datoteka je u .gitignore
cargo test -p smp-core --test smb_live -- --ignored --test-threads=1
```

Testovi idu na glavni server i, ako je `SMB_TEST2_HOST` postavljen, i na rezervni.
