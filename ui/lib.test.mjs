import { test } from "node:test";
import assert from "node:assert/strict";
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
  plural,
  passwordRequired,
} from "./lib.mjs";

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

test("plural picks Croatian forms (1 / 2-4 / 5+, 11-14 are 5+)", () => {
  const f = ["datoteka", "datoteke", "datoteka"];
  const forms = ["jedan", "dva", "pet"];
  const pick = (n) => plural(n, forms);
  assert.deepEqual([1, 21, 101].map(pick), ["jedan", "jedan", "jedan"]);
  assert.deepEqual([2, 3, 4, 22, 104].map(pick), ["dva", "dva", "dva", "dva", "dva"]);
  assert.deepEqual([0, 5, 11, 12, 13, 14, 111, 112, 25].map(pick), Array(9).fill("pet"));
  assert.equal(plural(2, f), "datoteke");
});

test("scanProgressText", () => {
  assert.equal(scanProgressText(null), "Skeniram… popisujem datoteke");
  assert.equal(scanProgressText({ seen: 0, read: 0, failed: 0 }), "Skeniram… popisujem datoteke");
  assert.equal(scanProgressText({ seen: 12340, read: 10, failed: 0 }), `Skeniram… 12${NB}340 datoteka`);
  assert.equal(scanProgressText({ seen: 1, read: 1, failed: 0 }), "Skeniram… 1 datoteka");
  assert.equal(scanProgressText({ seen: 3, read: 3, failed: 0 }), "Skeniram… 3 datoteke");
  assert.equal(scanProgressText({ seen: 5, read: 3, failed: 2 }), "Skeniram… 5 datoteka (nečitljivo: 2)");
});

test("scanDoneText covers success, cancel and error", () => {
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 59400, read: 100, failed: 2, removed: 1 }),
    `Katalog osvježen: 59${NB}400 datoteka. Novo ili promijenjeno: 100, nečitljivo: 2, uklonjeno: 1.`,
  );
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 22, read: 0, failed: 0, removed: 0 }),
    "Katalog osvježen: 22 datoteke. Novo ili promijenjeno: 0, nečitljivo: 0, uklonjeno: 0.",
  );
  assert.equal(scanDoneText({ ok: true, cancelled: true, cancel_reason: null }), "Skeniranje je prekinuto.");
  assert.equal(
    scanDoneText({ ok: false, cancelled: true, cancel_reason: "Postavke su promijenjene." }),
    "Postavke su promijenjene.",
  );
  assert.equal(
    scanDoneText({ ok: false, cancelled: false, error: "Disk nije dostupan." }),
    "Skeniranje nije uspjelo: Disk nije dostupan.",
  );
});

test("connMessage maps statuses to Croatian text", () => {
  assert.equal(connMessage("ok"), "Spojeno");
  assert.equal(connMessage("auth"), "Pogrešno korisničko ime ili lozinka");
  assert.match(connMessage("unreachable"), /Disk nije dostupan/);
  assert.equal(connMessage("secondary"), "Spojeno na rezervni server");
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
