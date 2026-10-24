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
  const KEY = /["'`]((?:language|player|conn|tabs|genres|merge|common|artists|albums|settings|scan|tray|err)\.[a-z_]+)["'`]/g;
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

test("mergeChoices lists known and existing genres except the source and Unknown", () => {
  const known = ["Chillout", "Drum & Bass", "Downtempo", "Trance"];
  const genres = [
    { key: "chillout", display: "Chillout", count: 3 },
    { key: "_none", display: "Unknown", count: 9 },
    { key: "lounge", display: "Lounge", count: 1 },
  ];
  assert.deepEqual(mergeChoices(known, genres, "chillout", ""), ["Downtempo", "Drum & Bass", "Lounge", "Trance"]);
  assert.deepEqual(mergeChoices(known, genres, "chillout", " d "), ["Downtempo", "Drum & Bass"]);
  assert.deepEqual(mergeChoices(known, genres, "lounge", "chill"), ["Chillout"]);
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
