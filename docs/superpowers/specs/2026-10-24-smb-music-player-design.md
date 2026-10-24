# SMB Music Player — dizajn (desktop: macOS + Windows)

Datum: 2026-10-24
Status: odobreno za macOS fazu

## 1. Namjera i zašto

Vlasnik ima veliku, uredno tagiranu glazbenu kolekciju na NAS-u (SMB share
`\\10.111.100.2\F\MP3`, zaštićen korisničkim imenom i lozinkom), složenu u
mape po izvođačima ili žanrovima. Sluša "po raspoloženju": nekad trance, nekad
chillout, jazz, domaće, dnb, metal, oldies, a nekad sve skupa.

Treba mu mali, neintruzivan desktop player koji:
- jednom (ili na zahtjev) pokupi popis svih pjesama s diska i zapamti ga,
- prikaže glazbu grupiranu po žanru,
- jednim klikom pusti nasumično iz odabranog žanra ili kombinacije žanrova,
- živi u trayu i preskače pjesmu jednim klikom.

**Uspjeh:** klikne žanr i glazba svira za par sekundi. Svaki put počinje drugom,
stvarno nasumičnom pjesmom. Nepoželjnu pjesmu preskoči jednim klikom u trayu,
bez otvaranja prozora. Player ne smeta dok radi.

## 2. Ciljevi

1. **Izvor:** SMB share. Host/IP, share i putanja mogu se mijenjati u
   postavkama (zadano `10.111.100.2`, share `F`, putanja `MP3`). Uz to idu
   korisničko ime, lozinka i opcionalna domena/radna grupa.
2. **Indeks:** rekurzivno skeniranje stabla i čitanje tagova (izvođač, album,
   naslov, žanr, trajanje, broj pjesme, ReplayGain ako postoji). Rezultat se
   sprema u lokalnu bazu (SQLite). Skeniranje se pokreće pri prvom pokretanju
   ili ručno ("Osvježi"). Ponovno skeniranje je inkrementalno: preskače
   datoteke kojima se nisu promijenili veličina i mtime.
3. **Reprodukcija se streama sa SMB-a**, bez preuzimanja cijele kolekcije.
   Pjesma se čita s read-ahead bufferom, a sljedeća pjesma se počinje učitavati
   prije kraja trenutne.
   - Crossfade je po defaultu **uključen, 5 s** (moja odluka).
   - Kad je crossfade isključen: nema umjetne tišine između pjesama, ali
     reprodukcija nije sample-točno gapless (nema trimanja LAME encoder
     delaya). Ako dekoder to već sam podržava, to je bonus (moja odluka).
   - I ručni "sljedeća" radi kratki fade (~0,5–1 s) da nema klika (moja
     odluka).
4. **Formati:** MP3, FLAC, OGG Vorbis (Opus u OGG-u je poželjan, ne obavezan).
5. **Nativni audio:** dekodiranje i izlaz idu kroz nativni audio stack, ne kroz
   web/HTML5 audio.
6. **Kontrole:** play/pauza, sljedeća, glasnoća, odabir izlaznog uređaja od
   onih koje sustav nudi (pamti se, uz fallback na zadani ako uređaj nestane).
   Crossfade se uključuje/isključuje, trajanje je 1–10 s.
7. **Žanrovi:** glavni pogled je popis žanrova s brojem pjesama. Može se
   odabrati jedan ili više žanrova, ili "Sve". Klik pokreće shuffle.
8. **Pravi random:**
   - pri svakom pokretanju odabira radi se svježe Fisher–Yates miješanje iz
     kriptografski seedanog RNG-a (OS entropija), bez fiksnog seeda i bez
     spremljenog redoslijeda;
   - prva pjesma je jednako nasumična kao i ostale; ako je ista kao prva pjesma
     prethodnog pokretanja za isti odabir, zamijeni se s drugom;
   - unutar jednog prolaza nema ponavljanja dok se ne iscrpi cijeli skup;
     zatim slijedi novo miješanje, pri čemu se pazi da prva pjesma novog
     prolaza nije zadnja odsvirana.
   - Testom se potvrđuje da su prve pjesme kroz 1000 pokretanja raspršene.
9. **Normalizacija žanrova:** slični nazivi automatski se spajaju
   (case/razmaci/interpunkcija, npr. "Drum & Bass" / "DnB" / "drum n bass",
   "Chill Out" / "Chillout"). Korisnik može ručno spojiti ili preimenovati
   žanr. Ručna pravila se spremaju lokalno i ne diraju tagove na disku.
   Pjesma s više žanrova (npr. "Trance; Chillout" ili više genre frameova)
   broji se u svakom od njih. Model ima zasebnu tablicu pjesma–žanr, a
   višestruki odabir uklanja duplikate prije miješanja (moja odluka).
   Pjesme bez žanra idu u "(bez žanra)", uz fallback na ime nadređene mape ako
   ona izgleda kao žanr (odluka graditelja kako to detektirati).
10. **Nečitljive datoteke:** ako datoteka nedostaje, oštećena je, ne da se
    dekodirati ili čitanje istekne, tiho se preskače i svira sljedeća. Datoteka
    se označi u bazi i nakon N uzastopnih neuspjeha isključi iz shufflea
    dok se ne napravi ponovno skeniranje.
11. **Normalizacija glasnoće (može se isključiti, uključena po defaultu):**
    - ReplayGain tag (track gain) primjenjuje se ako postoji;
    - ako ga nema, glasnoća se mjeri tijekom reprodukcije (EBU R128 /
      LUFS-procjena), rezultat se sprema u bazu i koristi od sljedećeg puta;
    - nema unaprijednog skeniranja glasnoće cijele kolekcije;
    - zaštita od clippinga (limiter ili ograničenje pojačanja).
12. **Tray / izbornička traka:**
    - lijevi klik na tray ikonu = **sljedeća pjesma** (glavna radnja);
    - desni klik otvara izbornik: trenutna pjesma (izvođač – naslov), pauza/
      nastavi, sljedeća, podizbornik "Žanr" za brzu promjenu raspoloženja,
      "Prikaži prozor", "Izlaz";
    - zatvaranje prozora skriva ga u tray, a ne gasi aplikaciju.
    - Napomena za macOS: lijevi klik na ikonu u menu baru po konvenciji otvara
      izbornik. Ako ga nije moguće pouzdano mapirati na "sljedeća", stavljaju
      se dvije ikone (⏭ i glavna) ili ⏭ kao prva stavka izbornika. Graditelj
      odlučuje prema tome što platforma dopušta, a cilj je preskakanje jednim
      klikom.
13. **Medijske tipke i OS kontrole:** play/pauza/sljedeća s tipkovnice,
    integracija s macOS Now Playing i Windows SMTC.
14. **Obavijest pri promjeni pjesme:** kratka sistemska obavijest
    (izvođač – naslov), može se isključiti, po defaultu isključena da ne bude
    intruzivno.
15. **Pokretanje sa sustavom:** opcija, po defaultu isključena.
16. **Ban:** "Nikad više ne puštaj ovu pjesmu" (iz trayа i prozora). Banane
    pjesme se mogu pregledati i odbanati u postavkama.
17. **Offline ponašanje:** ako share nije dostupan, katalog se i dalje
    prikazuje iz cachea. Aplikacija jasno kaže da disk nije dostupan i pokušava
    se ponovno spojiti. Kod pogrešne lozinke traži ispravak.
18. **Sigurnost vjerodajnica:** lozinka se sprema u OS keychain (macOS
    Keychain, Windows Credential Manager), nikad u čistu datoteku ili log.

## 3. Ne-ciljevi (za sada)

- Android: zaseban, drugi projekt nakon desktopa, na istim idejama (vidi §8).
- Uređivanje tagova na disku.
- Upravljanje playlistama, ocjene, statistike, last.fm, omoti albuma kao
  obavezni dio (omot se smije prikazati ako je trivijalno).
- Kopiranje ili sinkronizacija kolekcije lokalno.
- Bilo kakav cloud ili server komponenta.

## 4. Anti-ciljevi (bilo bi promašaj čak i da "radi")

- Prva pjesma shufflea je ponovljiva ili predvidiva.
- Aplikacija je teška i bučna: velik RAM/CPU u pozadini, iskačući prozori,
  fokus krađa, obavijesti po defaultu.
- Jedna loša datoteka zaustavlja reprodukciju ili izbacuje modalni dijalog.
- Lozinka u čistom tekstu na disku.
- Pokretanje aplikacije ponovno skenira cijeli disk.
- Reprodukcija ovisi o tome da OS ima montiran share (aplikacija se sama spaja
  preko SMB-a).

## 5. Ograničenja

- **Redoslijed (vlasnikova odluka):** prvo se radi i isporučuje macOS
  verzija. Windows dolazi poslije. Gradit će se preko GitHub Actions
  (Windows runner), a ako to ne bude moguće, vlasnik će kasnije pokrenuti
  Claude nativno na Windows stroju. Kod mora od početka biti cross-platform,
  bez macOS-only prečaca u jezgri, ali za Windows se u prvoj fazi ne
  isporučuje ništa osim CI builda ako on prođe.

- **Serveri:** glavni je `10.111.100.2`, Windows 10 (SMB 3.x, NTLMv2,
  signing moguć). Rezervni je `10.111.0.2`, Unraid (Samba), s istom glazbom.
  Port 445 na oba je dostupan s razvojnog Maca, pa se testira na pravim
  serverima.
- **Primarni i rezervni izvor (moja odluka):** postavke podržavaju glavni i
  opcionalni rezervni server. Svaki ima svoj host, share, putanju i
  vjerodajnice. Katalog se gradi s glavnog. Pjesma se identificira relativnom
  putanjom unutar korijena glazbe. Ako glavni nije dostupan, reprodukcija ide
  s rezervnog po istoj relativnoj putanji, a ako je tamo nema, pjesma se
  preskače. Pretpostavka je da je struktura mapa na oba ista; to treba
  potvrditi na stvarnim serverima prije implementacije fallbacka.

- macOS (Apple Silicon i Intel, macOS 12+) i Windows 10/11 x64.
- Kolekcija vjerojatno ima desetke tisuća datoteka. UI i baza moraju to
  podnijeti bez zastoja (virtualizirane liste).
- Mreža je LAN. Buffer mora preživjeti kratke zastoje od nekoliko sekundi.
- SMB2/3 s NTLM autentikacijom.
- Na razvojnom Macu trenutno nije instaliran Rust toolchain, treba ga
  instalirati (rustup).

## 6. Tehničke odluke (moj izbor; vlasnik je izbor stacka prepustio meni)

- **Okvir:** Tauri 2 (Rust backend, lagani web UI u sistemskom webviewu).
  Daje mali binarij, tray podršku i jedan kod za obje platforme.
- **Audio:** dekodiranje u Rustu (`symphonia`: MP3, FLAC, OGG Vorbis), izlaz
  preko `cpal` (CoreAudio / WASAPI), uključujući enumeraciju i odabir uređaja.
  Vlastiti mikser za crossfade (dvije dekodirane struje, equal-power krivulja),
  glasnoću i gain.
- **SMB klijent:** Rust SMB2/3 biblioteka koja ne ovisi o montiranju na OS-u.
  Graditelj bira konkretnu biblioteku nakon kratke provjere zrelosti. Ako
  nijedna nije dovoljno pouzdana, fallback je `libsmb2` preko FFI-ja.
- **Baza:** SQLite (`rusqlite`), u app data direktoriju.
- **Vjerodajnice:** `keyring` crate.
- **UI:** minimalan; frontend framework bira graditelj (dovoljan je i vanilla
  ili Svelte). Svijetla/tamna tema prema sustavu.
- **Pakiranje:** `.dmg` za macOS, `.msi`/`.exe` za Windows. Potpisivanje i
  notarizacija nisu nužni za osobnu upotrebu. Graditelj dokumentira kako
  zaobići Gatekeeper/SmartScreen.

## 7. Prepušteno graditelju

- Raspored prozora i vizualni stil, uz uvjet: kompaktno, jedan glavni pogled
  sa žanrovima, mini kontrole, sporedni pogled izvođač/album.
- Veličina read-ahead buffera i strategija pretpunjenja.
- Točan algoritam spajanja žanrova i detekcija "mapa = žanr".
- N za isključivanje nečitljivih datoteka (prijedlog: 3).
- Način paralelizacije skeniranja i prikaz napretka.
- Ikone.

## 8. Napomena za budući Android projekt

Isti model: SMB indeks u lokalnoj bazi, shuffle po žanru, crossfade, preskakanje
neispravnih datoteka. Prijedlog stacka: Kotlin + Media3/ExoPlayer, `smbj` ili
`jcifs-ng`, MediaSession (kontrole na zaključanom zaslonu i u obavijestima),
Android Keystore. Shuffle i normalizacija žanrova trebaju se ponašati jednako
kao na desktopu, pa ih vrijedi opisati kao jasnu specifikaciju, ne samo kao
kod.
