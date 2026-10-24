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
  mergeChoices,
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

test("plural picks the English singular only for exactly one", () => {
  const forms = ["track", "tracks"];
  assert.equal(plural(1, forms), "track");
  assert.deepEqual([0, 2, 11, 21, 101].map((n) => plural(n, forms)), Array(5).fill("tracks"));
});

test("scanProgressText", () => {
  assert.equal(scanProgressText(null), "Scanning… listing files");
  assert.equal(scanProgressText({ seen: 0, read: 0, failed: 0 }), "Scanning… listing files");
  assert.equal(scanProgressText({ seen: 12340, read: 10, failed: 0 }), `Scanning… 12${NB}340 files`);
  assert.equal(scanProgressText({ seen: 1, read: 1, failed: 0 }), "Scanning… 1 file");
  assert.equal(scanProgressText({ seen: 3, read: 3, failed: 0 }), "Scanning… 3 files");
  assert.equal(scanProgressText({ seen: 5, read: 3, failed: 2 }), "Scanning… 5 files (unreadable: 2)");
});

test("scanDoneText covers success, cancel and error", () => {
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 59400, read: 100, failed: 2, removed: 1 }),
    `Library refreshed: 59${NB}400 files. New or changed: 100, unreadable: 2, removed: 1.`,
  );
  assert.equal(
    scanDoneText({ ok: true, cancelled: false, total: 1, read: 0, failed: 0, removed: 0 }),
    "Library refreshed: 1 file. New or changed: 0, unreadable: 0, removed: 0.",
  );
  assert.equal(scanDoneText({ ok: true, cancelled: true, cancel_reason: null }), "Scan cancelled.");
  assert.equal(
    scanDoneText({ ok: false, cancelled: true, cancel_reason: "The server settings changed." }),
    "The server settings changed.",
  );
  assert.equal(
    scanDoneText({ ok: false, cancelled: false, error: "source unreachable" }),
    "Scan failed: source unreachable",
  );
});

test("connMessage maps statuses to the backend's English text", () => {
  assert.equal(connMessage("ok"), "Connected");
  assert.equal(connMessage("auth"), "Wrong username or password");
  assert.equal(connMessage("unreachable"), "The music share is unreachable. Is WireGuard/VPN on?");
  assert.equal(connMessage("secondary"), "Connected to the backup server");
  assert.equal(connMessage("nope"), "");
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
