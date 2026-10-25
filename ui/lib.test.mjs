import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import {
  formatCount,
  trackLabel,
  foldForSearch,
  filterGenres,
  limitMatches,
  debounce,
  scanProgressText,
  scanDoneText,
  connMessage,
  serverFromForm,
  missingUsername,
  passwordRequired,
  mergeChoices,
  pluralCategory,
  PLURAL_CATEGORIES,
  makeT,
  albumLabel,
  reasonText,
  FAIL_CODES,
  unreadableTabLabel,
  copyAllText,
  formatWhen,
  formatTime,
  positionText,
  progressFraction,
  queueFilter,
  exportFileName,
  windowRange,
  pageOffsets,
  sameItems,
  keepExpanded,
  genresSummary,
  patchPlan,
  retryDelay,
  sliderFollows,
  libraryName,
  libraryFromForm,
  statusTooltip,
} from "./lib.mjs";

const read = (rel) => readFileSync(new URL(rel, import.meta.url), "utf8");
const LOCALES = Object.fromEntries(
  readdirSync(new URL("./locales/", import.meta.url))
    .filter((f) => f.endsWith(".json"))
    .map((f) => [f.slice(0, -5), JSON.parse(read(`./locales/${f}`))]),
);
const en = makeT("en", LOCALES.en, LOCALES.en);
const hr = makeT("hr", LOCALES.hr, LOCALES.en);

const NB = " ";

test("formatCount groups thousands with a no-break space", () => {
  assert.equal(formatCount(0), "0");
  assert.equal(formatCount(999), "999");
  assert.equal(formatCount(1000), `1${NB}000`);
  assert.equal(formatCount(12340), `12${NB}340`);
  assert.equal(formatCount(1234567), `1${NB}234${NB}567`);
});

test("trackLabel shows artist – title, falls back gracefully, dash when idle", () => {
  assert.equal(trackLabel(null), "—");
  assert.equal(trackLabel({ artist: "Moby", title: "Porcelain" }), "Moby – Porcelain");
  assert.equal(trackLabel({ artist: "", title: "Porcelain" }), "Porcelain");
  assert.equal(trackLabel({ artist: "Moby", title: "" }), "Moby");
  assert.equal(trackLabel({ artist: "", title: "" }), "—");
});

test("trackLabel ends with the year when there is one", () => {
  assert.equal(trackLabel({ artist: "Moby", title: "Porcelain", year: 1999 }), "Moby – Porcelain (1999)");
  assert.equal(trackLabel({ artist: "", title: "Porcelain", year: 1999 }), "Porcelain (1999)");
  assert.equal(trackLabel({ artist: "Moby", title: "Porcelain", year: null }), "Moby – Porcelain");
  assert.equal(trackLabel({ artist: "", title: "", year: 1999 }), "—");
});

test("albumLabel shows the year next to the album, without digit grouping", () => {
  assert.equal(albumLabel({ name: "Play", year: 1999 }, en), "Play (1999)");
  assert.equal(albumLabel({ name: "Play", year: null }, en), "Play");
  assert.equal(albumLabel({ name: "", year: 2001 }, en), "(no album) (2001)");
  assert.equal(albumLabel({ name: "", year: null }, hr), "(bez albuma)");
});

test("unreadable: reasons, tab label and the copy-all text", () => {
  assert.deepEqual(FAIL_CODES, ["not_found", "timeout", "unreachable", "decode", "unsupported", "tag", "io"]);
  for (const code of [...FAIL_CODES, "unknown"]) {
    for (const lang of Object.keys(LOCALES)) assert.ok(`unreadable.reason_${code}` in LOCALES[lang], `${lang} ${code}`);
  }
  assert.equal(reasonText("timeout", en), "The server did not answer in time");
  assert.equal(reasonText(null, en), "Unknown reason");
  assert.equal(reasonText("weird", hr), "Nepoznat razlog");
  assert.equal(unreadableTabLabel(0, en), "Unreadable");
  assert.equal(unreadableTabLabel(1234, en), `Unreadable (1${NB}234)`);
  assert.equal(unreadableTabLabel(3, hr), "Nečitljivo (3)");
  const rows = [
    { path: "smb://h/F/MP3/a b.mp3", url: "smb://h/F/MP3/a%20b.mp3", code: "not_found", message: "source error: not found" },
    { path: "smb://h/F/MP3/c.flac", url: "smb://h/F/MP3/c.flac", code: null, message: null },
  ];
  // The valid URL is copied, not the readable path.
  assert.equal(
    copyAllText(rows, en),
    "smb://h/F/MP3/a%20b.mp3\tFile not found (source error: not found)\nsmb://h/F/MP3/c.flac\tUnknown reason",
  );
  assert.equal(copyAllText([], en), "");
});

test("formatWhen shows a date for Unix seconds and nothing without one", () => {
  assert.equal(formatWhen(null, "en"), "");
  const text = formatWhen(1_700_000_000, "en");
  assert.match(text, /2023/);
});

test("foldForSearch ignores case and diacritics", () => {
  assert.equal(foldForSearch("  Čćžšđ ROCK "), "cczsd rock");
});

test("filterGenres matches substrings regardless of case and diacritics", () => {
  const genres = [
    { key: "a", display: "Chillout", count: 3 },
    { key: "b", display: "Drum & Bass", count: 5 },
    { key: "c", display: "Zabavna glazba Čvrsto", count: 1 },
  ];
  assert.deepEqual(filterGenres(genres, ""), genres);
  assert.deepEqual(filterGenres(genres, "  "), genres);
  assert.deepEqual(filterGenres(genres, "CHILL").map((g) => g.key), ["a"]);
  assert.deepEqual(filterGenres(genres, "cvrs").map((g) => g.key), ["c"]);
  assert.deepEqual(filterGenres(genres, "zzz"), []);
});

test("limitMatches caps results and reports the total", () => {
  const names = Array.from({ length: 1000 }, (_, i) => `Artist ${i}`);
  const all = limitMatches(names, "", 300);
  assert.equal(all.items.length, 300);
  assert.equal(all.total, 1000);
  assert.equal(all.truncated, true);

  const few = limitMatches(names, "artist 99", 300);
  assert.deepEqual(few.items, ["Artist 99", "Artist 990", "Artist 991", "Artist 992",
    "Artist 993", "Artist 994", "Artist 995", "Artist 996", "Artist 997", "Artist 998",
    "Artist 999"]);
  assert.equal(few.truncated, false);
  assert.equal(limitMatches(["Đorđe"], "dord", 300).total, 1);
});

test("debounce runs once with the last arguments after the quiet period", async () => {
  const calls = [];
  const d = debounce((x) => calls.push(x), 20);
  d(1);
  d(2);
  d(3);
  assert.deepEqual(calls, []);
  await new Promise((r) => setTimeout(r, 50));
  assert.deepEqual(calls, [3]);
  d(4);
  await new Promise((r) => setTimeout(r, 50));
  assert.deepEqual(calls, [3, 4]);
});

test("locales: en and hr exist and have exactly the same keys", () => {
  assert.ok(LOCALES.en && LOCALES.hr);
  for (const [lang, dict] of Object.entries(LOCALES)) {
    assert.deepEqual(Object.keys(dict).sort(), Object.keys(LOCALES.en).sort(), `${lang} vs en`);
  }
});

test("locales: plural forms match the language's categories, placeholders match en", () => {
  const placeholders = (v) =>
    [...new Set([...(typeof v === "string" ? v : Object.values(v).join(" ")).matchAll(/\{(\w+)\}/g)].map((m) => m[1]))].sort();
  for (const [lang, dict] of Object.entries(LOCALES)) {
    assert.ok(PLURAL_CATEGORIES[lang], `plural rule for ${lang}`);
    for (const [key, value] of Object.entries(dict)) {
      const base = LOCALES.en[key];
      assert.equal(typeof value, typeof base, `${lang} ${key} type`);
      if (typeof value === "object") {
        assert.deepEqual(Object.keys(value).sort(), [...PLURAL_CATEGORIES[lang]].sort(), `${lang} ${key}`);
      } else {
        assert.ok(value.trim(), `${lang} ${key} is empty`);
      }
      assert.deepEqual(placeholders(value), placeholders(base), `${lang} ${key} placeholders`);
    }
  }
});

test("every translation key used by the UI and the Rust code exists", () => {
  const KEY = /["'`]((?:language|player|conn|tabs|genres|merge|common|artists|albums|settings|scan|tray|err|unreadable|playlist|menu|about|libraries)\.[a-z_]+)["'`]/g;
  const sources = [read("./app.js"), read("./lib.mjs"), read("./index.html")];
  const rustDir = new URL("../src-tauri/src/", import.meta.url);
  for (const f of readdirSync(rustDir).filter((f) => f.endsWith(".rs"))) sources.push(read(`../src-tauri/src/${f}`));
  const fileName = /\.(json|db|js|mjs|html|css|rs|png)$/;
  const used = new Set(
    sources.flatMap((src) => [...src.matchAll(KEY)].map((m) => m[1])).filter((k) => !fileName.test(k)),
  );
  assert.ok(used.size > 50, `found ${used.size} keys`);
  for (const key of used) assert.ok(key in LOCALES.en, `missing key ${key}`);
});

test("pluralCategory: en one/other, hr one/few/other with 11-14 as other", () => {
  assert.equal(pluralCategory("en", 1), "one");
  assert.deepEqual([0, 2, 11, 21].map((n) => pluralCategory("en", n)), Array(4).fill("other"));
  const hrCat = (n) => pluralCategory("hr", n);
  assert.deepEqual([1, 21, 101].map(hrCat), Array(3).fill("one"));
  assert.deepEqual([2, 3, 4, 22, 104].map(hrCat), Array(5).fill("few"));
  assert.deepEqual([0, 5, 11, 12, 13, 14, 111, 112, 25].map(hrCat), Array(9).fill("other"));
  assert.equal(pluralCategory("xx", 1), "one", "unknown language falls back to en");
});

test("t substitutes params, formats numbers and falls back to en, then the key", () => {
  assert.equal(en("genres.play", { name: "Jazz" }), "Play: Jazz");
  assert.equal(en("scan.files", { n: 1 }), "1 file");
  assert.equal(en("scan.files", { n: 12340 }), `12${NB}340 files`);
  assert.equal(hr("scan.files", { n: 3 }), "3 datoteke");
  assert.equal(hr("scan.files", { n: 12 }), "12 datoteka");
  const partial = makeT("hr", { "tabs.genres": "Žanrovi" }, LOCALES.en);
  assert.equal(partial("tabs.genres"), "Žanrovi");
  assert.equal(partial("tabs.artists"), "Artists");
  assert.equal(partial("no.such.key"), "no.such.key");
});

test("scanProgressText", () => {
  assert.equal(scanProgressText(null, en), "Scanning… listing files");
  assert.equal(scanProgressText({ seen: 0, read: 0, failed: 0 }, en), "Scanning… listing files");
  assert.equal(scanProgressText({ seen: 12340, read: 10, failed: 0 }, en), `Scanning… 12${NB}340 files`);
  assert.equal(scanProgressText({ seen: 1, read: 1, failed: 0 }, en), "Scanning… 1 file");
  assert.equal(scanProgressText({ seen: 5, read: 3, failed: 2 }, en), "Scanning… 5 files (unreadable: 2)");
  assert.equal(scanProgressText({ seen: 3, read: 3, failed: 0 }, hr), "Skeniram… 3 datoteke");
  assert.equal(scanProgressText({ seen: 5, read: 3, failed: 2 }, hr), "Skeniram… 5 datoteka (nečitljivo: 2)");
});

test("scanDoneText covers success, cancel and error", () => {
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 59400, read: 100, failed: 2, removed: 1 }, en),
    `Library refreshed: 59${NB}400 files. New or changed: 100, unreadable: 2, removed: 1.`,
  );
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 1, read: 0, failed: 0, removed: 0 }, en),
    "Library refreshed: 1 file. New or changed: 0, unreadable: 0, removed: 0.",
  );
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 22, read: 0, failed: 0, removed: 0 }, hr),
    "Katalog osvježen: 22 datoteke. Novo ili promijenjeno: 0, nečitljivo: 0, uklonjeno: 0.",
  );
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 1, read: 0, failed: 0, removed: 0, skipped_dirs: 2 }, en),
    "Library refreshed: 1 file. New or changed: 0, unreadable: 0, removed: 0. 2 folders could not be read; their tracks were kept.",
  );
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 1, read: 0, failed: 0, removed: 0, skipped_dirs: 1 }, hr),
    "Katalog osvježen: 1 datoteka. Novo ili promijenjeno: 0, nečitljivo: 0, uklonjeno: 0. 1 mapa nije se mogla pročitati; njezine pjesme su zadržane.",
  );
  assert.equal(scanDoneText({ ok: true, cancelled: true, cancel_reason: null }, en), "Scan cancelled.");
  assert.equal(scanDoneText({ ok: true, cancelled: true, cancel_reason: null }, hr), "Skeniranje je prekinuto.");
  assert.equal(
    scanDoneText({ ok: false, cancelled: true, cancel_reason: "The server settings changed." }, en),
    "The server settings changed.",
  );
  assert.equal(
    scanDoneText({ ok: false, cancelled: false, error: "source unreachable" }, en),
    "Scan failed: source unreachable",
  );
});

test("connMessage maps statuses to the locale's text", () => {
  assert.equal(connMessage("ok", en), "Connected");
  assert.equal(connMessage("auth", en), "Wrong username or password");
  assert.equal(connMessage("unreachable", en), "The music share is unreachable. Is WireGuard/VPN on?");
  assert.equal(connMessage("secondary", en), "Connected to the backup server");
  assert.equal(connMessage("auth", hr), "Pogrešno korisničko ime ili lozinka");
  assert.equal(connMessage("nope", en), "");
});

test("app.js never shadows the translator t", () => {
  // The one module-level translator binding is allowed.
  const src = read("./app.js").replace("let t = (key) => key;", "");
  const shadow = /\b(?:const|let|var)\s+t\b|\bfor\s*\(\s*(?:const|let|var)\s+t\b|\(\s*t\s*\)\s*=>|\(\s*t\s*,|,\s*t\s*\)\s*=>|function\s*\w*\s*\([^)]*\bt\b[^)]*\)/g;
  const hits = [...src.matchAll(shadow)].map((m) => m[0]);
  assert.deepEqual(hits, [], "a local `t` hides the translator");
});

test("mergeChoices: genres with tracks first; other known genres only when searching", () => {
  const known = ["Chillout", "Drum & bass", "Downtempo", "Trance", "Lounge"];
  const genres = [
    { key: "chillout", display: "Chillout", count: 3 },
    { key: "_none", display: "Unknown", count: 9 },
    { key: "lounge", display: "Lounge", count: 1 },
    { key: "trance", display: "Trance", count: 2 },
  ];
  assert.deepEqual(mergeChoices(known, genres, "chillout", ""), ["Lounge", "Trance"]);
  assert.deepEqual(mergeChoices(known, genres, "chillout", "n"), ["Lounge", "Trance", "Downtempo"]);
  assert.deepEqual(mergeChoices(known, genres, "lounge", "chill"), ["Chillout"]);
  assert.deepEqual(mergeChoices(known, genres, "lounge", "dow"), ["Downtempo"]);
  assert.deepEqual(mergeChoices([], [], "x", "zzz"), []);
});

test("serverFromForm trims fields and turns an empty domain into null", () => {
  assert.deepEqual(
    serverFromForm({ host: " 10.111.100.2 ", share: "F", root: "/MP3 ", username: " music ", domain: " " }),
    { host: "10.111.100.2", share: "F", root: "/MP3", username: "music", domain: null },
  );
  assert.equal(serverFromForm({ host: "h", share: "s", root: "", username: "u", domain: "WG" }).domain, "WG");
});

test("missingUsername", () => {
  assert.equal(missingUsername({ username: "" }), true);
  assert.equal(missingUsername({ username: "  " }), true);
  assert.equal(missingUsername({ username: "x" }), false);
});

test("passwordRequired when the keychain account (host/username) or share is new", () => {
  const saved = { host: "10.111.100.2", share: "F", root: "MP3", username: "music", domain: null };
  assert.equal(passwordRequired(saved, { ...saved }), false);
  assert.equal(passwordRequired(saved, { ...saved, root: "Other", domain: "WG" }), false);
  assert.equal(passwordRequired(saved, { ...saved, host: "10.111.0.2" }), true);
  assert.equal(passwordRequired(saved, { ...saved, username: "other" }), true);
  assert.equal(passwordRequired(saved, { ...saved, share: "G" }), true);
  assert.equal(passwordRequired(null, saved), true, "server not saved before");
  assert.equal(passwordRequired({ ...saved, username: "" }, saved), true, "never configured");
});

test("formatTime: m:ss, h:mm:ss for an hour or more", () => {
  assert.equal(formatTime(0), "0:00");
  assert.equal(formatTime(5_999), "0:05");
  assert.equal(formatTime(65_000), "1:05");
  assert.equal(formatTime(599_000), "9:59");
  assert.equal(formatTime(3_600_000), "1:00:00");
  assert.equal(formatTime(3_725_000), "1:02:05");
  assert.equal(formatTime(65_000, true), "0:01:05", "hours shown to match a long total");
  assert.equal(formatTime(-5), "0:00");
  assert.equal(formatTime(null), "0:00");
});

test("positionText: elapsed / total, only elapsed when the length is unknown", () => {
  assert.equal(positionText(83_000, 296_000), "1:23 / 4:56");
  assert.equal(positionText(83_000, null), "1:23");
  assert.equal(positionText(83_000, 0), "1:23");
  assert.equal(positionText(65_000, 3_725_000), "0:01:05 / 1:02:05");
  assert.equal(positionText(300_000, 296_000), "4:56 / 4:56", "never past the end");
});

test("progressFraction: 0..1, null when the length is unknown", () => {
  assert.equal(progressFraction(0, 200_000), 0);
  assert.equal(progressFraction(50_000, 200_000), 0.25);
  assert.equal(progressFraction(300_000, 200_000), 1);
  assert.equal(progressFraction(10, null), null);
  assert.equal(progressFraction(10, 0), null);
});

test("queueFilter: only what is set, years as a sorted range", () => {
  assert.equal(queueFilter({ text: "  ", genres: [], from: null, to: null, unknownYear: false }), null);
  assert.deepEqual(queueFilter({ text: " abba ", genres: [] }), { text: "abba" });
  assert.deepEqual(queueFilter({ genres: ["trance", "house"] }), { genres: ["trance", "house"] });
  assert.deepEqual(queueFilter({ from: 1990, to: 1999 }), { years: [1990, 1999] });
  assert.deepEqual(queueFilter({ from: 1999, to: 1990 }), { years: [1990, 1999] });
  assert.deepEqual(queueFilter({ from: 1990 }), { years: [1990, 9999] });
  assert.deepEqual(queueFilter({ to: 1985 }), { years: [0, 1985] });
  assert.deepEqual(queueFilter({ unknownYear: true }), { unknown_year: true });
  assert.deepEqual(queueFilter({ from: 2000, unknownYear: true }), { years: [2000, 9999], unknown_year: true });
});

test("exportFileName: name, local date, .m3u8", () => {
  const d = new Date(2026, 9, 5, 23, 59);
  assert.equal(exportFileName("Trance + Chillout", d), "Trance + Chillout 2026-10-05.m3u8");
  assert.equal(exportFileName("", d), "Playlist 2026-10-05.m3u8");
});

test("windowRange: the rows in view plus a margin, within the list", () => {
  // List starts 100 px below the top of a 300 px view, rows of 30 px.
  assert.deepEqual(windowRange(100, 300, 30, 1000, 5), { start: 0, end: 12 });
  // Scrolled: the list's top is 3000 px above the view.
  assert.deepEqual(windowRange(-3000, 300, 30, 1000, 5), { start: 95, end: 115 });
  assert.deepEqual(windowRange(-29_900, 300, 30, 1000, 5), { start: 991, end: 1000 });
  assert.deepEqual(windowRange(0, 300, 30, 0, 5), { start: 0, end: 0 });
});

test("pageOffsets: the pages covering a range", () => {
  assert.deepEqual(pageOffsets(0, 12, 200), [0]);
  assert.deepEqual(pageOffsets(190, 215, 200), [0, 200]);
  assert.deepEqual(pageOffsets(400, 400, 200), []);
});

test("artists: an unchanged list is left alone, open artists stay open", () => {
  assert.ok(sameItems(["A", "B"], ["A", "B"]));
  assert.ok(!sameItems(["A", "B"], ["A", "C"]));
  assert.ok(!sameItems(["A"], ["A", "B"]));
  assert.ok(!sameItems(null, ["A"]));
  const open = new Set(["Abba", "Gone", ""]);
  assert.deepEqual([...keepExpanded(open, ["", "Abba", "Queen"])].sort(), ["", "Abba"]);
});

test("genresSummary: all, one name, or a count", () => {
  const genres = [{ key: "trance", display: "Trance" }, { key: "house", display: "House" }];
  assert.equal(genresSummary([], genres, en), en("playlist.genres_all"));
  assert.equal(genresSummary(["house"], genres, en), "House");
  assert.equal(genresSummary(["house", "trance"], genres, en), en("playlist.genres_n", { n: 2 }));
});

test("debounce: flush runs a pending call now, once", async () => {
  const seen = [];
  const f = debounce((x) => seen.push(x), 30);
  f(1);
  f(2);
  f.flush();
  assert.deepEqual(seen, [2]);
  await new Promise((r) => setTimeout(r, 50));
  assert.deepEqual(seen, [2], "the timer was cancelled");
  f.flush();
  assert.deepEqual(seen, [2], "nothing pending: nothing runs");
  f(3);
  await new Promise((r) => setTimeout(r, 50));
  assert.deepEqual(seen, [2, 3]);
});

test("patchPlan: only rows that changed are replaced", () => {
  assert.deepEqual(patchPlan(["a", "b", "c"], ["a", "b", "c"]), { replace: [], add: 0, remove: 0 });
  assert.deepEqual(patchPlan(["a", "b", "c"], ["a", "x", "c"]), { replace: [1], add: 0, remove: 0 });
  assert.deepEqual(patchPlan(["a", "b"], ["a", "b", "c", "d"]), { replace: [], add: 2, remove: 0 });
  assert.deepEqual(patchPlan(["a", "b", "c"], ["a"]), { replace: [], add: 0, remove: 2 });
  assert.deepEqual(patchPlan([], ["a"]), { replace: [], add: 1, remove: 0 });
});

test("retryDelay: backs off, then gives up", () => {
  assert.deepEqual([1, 2, 3, 4, 5].map(retryDelay), [500, 1000, 2000, 4000, 8000]);
  assert.equal(retryDelay(6), null);
});

test("sliderFollows: a volume from elsewhere moves the slider unless it was just used", () => {
  assert.equal(sliderFollows(null, 1000), true, "never touched");
  assert.equal(sliderFollows(1000, 1200), false, "still being dragged");
  assert.equal(sliderFollows(1000, 1600), true, "left alone long enough");
});

test("connMessage words a missing folder", () => {
  assert.equal(connMessage("missing", en), "Music folder not found. Is the disk connected?");
  assert.equal(connMessage("missing", hr), "Mapa s glazbom nije pronađena. Je li disk spojen?");
});

test("libraryName: its own name, else the main server, the folder or the host", () => {
  const local = (path) => ({ type: "local", path });
  assert.equal(libraryName({ id: 3, name: " Disk ", source: local("/x") }, en), "Disk");
  assert.equal(libraryName({ id: 1, name: "" }, en), "Main server");
  assert.equal(libraryName({ id: 1, name: "" }, hr), "Glavni server");
  assert.equal(libraryName({ id: 2, name: "", source: local("/Volumes/USB Disk/") }, en), "USB Disk");
  assert.equal(libraryName({ id: 2, name: "", source: local("D:\\Glazba\\Čista") }, en), "Čista");
  const smb = { type: "smb", primary: { host: " nas.local " }, secondary: null };
  assert.equal(libraryName({ id: 4, name: "", source: smb }, en), "nas.local");
  assert.equal(libraryName({ id: 5, name: "" }, en), "Library 5");
});

test("libraryFromForm builds the backend's library", () => {
  const smbForm = { host: " h ", share: "F", root: "MP3", username: " u ", domain: "" };
  assert.deepEqual(
    libraryFromForm({ id: 0, name: " NAS ", enabled: true, type: "smb", primary: smbForm, secondary: null }),
    {
      id: 0,
      name: "NAS",
      enabled: true,
      source: { type: "smb", primary: { host: "h", share: "F", root: "MP3", username: "u", domain: null }, secondary: null },
    },
  );
  assert.deepEqual(
    libraryFromForm({ id: 4, name: "", enabled: false, type: "local", path: " /Volumes/Glazba čž " }),
    { id: 4, name: "", enabled: false, source: { type: "local", path: "/Volumes/Glazba čž" } },
  );
});

test("statusTooltip lists every library's state", () => {
  const payload = {
    status: "missing",
    libraries: [
      { id: 1, name: "", status: "ok" },
      { id: 2, name: "USB", status: "missing" },
    ],
  };
  assert.equal(
    statusTooltip(payload, en),
    "Main server: Connected\nUSB: Music folder not found. Is the disk connected?",
  );
  // Names of unnamed libraries come from the list of libraries.
  const known = [{ id: 2, name: "", source: { type: "local", path: "/Volumes/Disk" } }];
  assert.equal(
    statusTooltip({ ...payload, libraries: [{ id: 2, name: "", status: "ok" }] }, en, known),
    "Disk: Connected",
  );
  assert.equal(statusTooltip(null, en), "");
});

test("scan texts name the library when several are scanned", () => {
  const p = { seen: 3, read: 3, failed: 0, library: { id: 2, name: "USB" }, library_index: 2, library_count: 3 };
  assert.equal(scanProgressText(p, en), "USB (2/3): Scanning… 3 files");
  assert.equal(scanProgressText({ ...p, library_count: 1 }, en), "Scanning… 3 files");
  const done = { ok: true, cancelled: false, total: 2, read: 0, failed: 0, removed: 0 };
  assert.equal(
    scanDoneText({ ...done, offline: [{ id: 2, name: "USB", error: "x" }, { id: 1, name: "", error: "y" }] }, en),
    "Library refreshed: 2 files. New or changed: 0, unreadable: 0, removed: 0. Not reachable, their tracks were kept: USB, Main server.",
  );
  assert.equal(
    scanDoneText({ ...done, looks_empty: [{ id: 2, name: "USB" }] }, en),
    "Library refreshed: 2 files. New or changed: 0, unreadable: 0, removed: 0. The folder looks empty, nothing removed: USB.",
  );
  assert.equal(
    scanDoneText({ ...done, looks_empty: [{ id: 2, name: "USB" }] }, hr),
    "Katalog osvježen: 2 datoteke. Novo ili promijenjeno: 0, nečitljivo: 0, uklonjeno: 0. Mapa izgleda prazno, ništa nije uklonjeno: USB.",
  );
  assert.equal(
    scanDoneText({ ...done, from_backup: [{ id: 1, name: "" }] }, hr),
    "Katalog osvježen: 2 datoteke. Novo ili promijenjeno: 0, nečitljivo: 0, uklonjeno: 0. Skenirano s rezervnog servera, ništa nije uklonjeno: Glavni server.",
  );
});
