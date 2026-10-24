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
- Svaka mrežna operacija ima timeout 10 s (`SourceError::Timeout`); prekinuta
  veza se jednom ponovno uspostavi, a tek onda se vraća greška.
- Nečitljive podmape se preskaču (log), greška je samo ako se ne može
  izlistati korijen.

Live testovi (samo čitanje, ništa se ne piše na servere):

```sh
cp .env.test.example .env.test   # upiši podatke; datoteka je u .gitignore
cargo test -p smp-core --test smb_live -- --ignored --test-threads=1
```

Testovi idu na glavni server i, ako je `SMB_TEST2_HOST` postavljen, i na rezervni.
