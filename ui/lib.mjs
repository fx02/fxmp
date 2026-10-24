// Pure helpers for the UI (no DOM, no Tauri), so `node --test ui/*.test.mjs` can
// check them.

const NBSP = " ";

/** 12340 -> "12 340" (no-break space between thousands). */
export function formatCount(n) {
  return String(Math.trunc(n)).replace(/\B(?=(\d{3})+(?!\d))/g, NBSP);
}

/** "Artist – Title" for the now-playing bar; "—" when nothing plays. */
export function trackLabel(np) {
  if (!np) return "—";
  const parts = [np.artist, np.title].map((s) => (s || "").trim()).filter(Boolean);
  return parts.length ? parts.join(" – ") : "—";
}

/** Lowercase, no diacritics, trimmed: "Čvrsto" and "cvrsto" match. */
export function foldForSearch(s) {
  return s
    .normalize("NFD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/đ/g, "d")
    .replace(/Đ/g, "D")
    .toLowerCase()
    .trim();
}

/** Genres whose display name contains `query` (all of them for an empty query). */
export function filterGenres(genres, query) {
  const q = foldForSearch(query);
  if (!q) return genres;
  return genres.filter((g) => foldForSearch(g.display).includes(q));
}

/** Names containing `query`, at most `limit` of them, plus the total match count. */
export function limitMatches(names, query, limit) {
  const q = foldForSearch(query);
  const items = [];
  let total = 0;
  for (const name of names) {
    if (q && !foldForSearch(name).includes(q)) continue;
    total += 1;
    if (items.length < limit) items.push(name);
  }
  return { items, total, truncated: total > items.length };
}

/** Calls `fn` with the latest arguments once calls have stopped for `ms`. */
export function debounce(fn, ms) {
  let timer = null;
  return (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
}

/** English plural: `forms` = [singular, plural]; only exactly 1 is singular. */
export function plural(n, forms) {
  return n === 1 ? forms[0] : forms[1];
}

const FILES = ["file", "files"];

export function scanProgressText(p) {
  // Nothing is counted while the share is still being listed.
  if (!p || !p.seen) return "Scanning… listing files";
  let text = `Scanning… ${formatCount(p.seen)} ${plural(p.seen, FILES)}`;
  if (p.failed) text += ` (unreadable: ${formatCount(p.failed)})`;
  return text;
}

/** Text for the `scan-done` event payload. */
export function scanDoneText(d) {
  if (d.cancelled) return d.cancel_reason || "Scan cancelled.";
  if (d.error) return `Scan failed: ${d.error}`;
  return (
    `Library refreshed: ${formatCount(d.total)} ${plural(d.total, FILES)}. ` +
    `New or changed: ${formatCount(d.read)}, ` +
    `unreadable: ${formatCount(d.failed)}, removed: ${formatCount(d.removed)}.`
  );
}

// Same wording as the backend's `ConnStatus::message`.
const CONN_MESSAGES = {
  ok: "Connected",
  secondary: "Connected to the backup server",
  unreachable: "The music share is unreachable. Is WireGuard/VPN on?",
  auth: "Wrong username or password",
};

export function connMessage(status) {
  return CONN_MESSAGES[status] || "";
}

const UNKNOWN_KEY = "_none";

/**
 * Genres that genre `fromKey` can be merged into: the known genres plus the ones in
 * the library, without `fromKey` itself and without Unknown, sorted, matching `query`.
 */
export function mergeChoices(knownNames, genres, fromKey, query) {
  const from = genres.find((g) => g.key === fromKey);
  const names = new Set(knownNames);
  for (const g of genres) if (g.key !== UNKNOWN_KEY) names.add(g.display);
  if (from) names.delete(from.display);
  const q = foldForSearch(query);
  return [...names]
    .filter((n) => !q || foldForSearch(n).includes(q))
    .sort((a, b) => a.localeCompare(b, "en", { sensitivity: "base" }));
}

/** Form values -> backend `ServerConfig`. */
export function serverFromForm(f) {
  const domain = (f.domain || "").trim();
  return {
    host: (f.host || "").trim(),
    share: (f.share || "").trim(),
    root: (f.root || "").trim(),
    username: (f.username || "").trim(),
    domain: domain || null,
  };
}

export function missingUsername(server) {
  return !(server.username || "").trim();
}

/**
 * Whether saving `form` needs a typed password: the password is stored per
 * host/username, so a new account (or share) has none to keep.
 */
export function passwordRequired(saved, form) {
  if (!saved || missingUsername(saved)) return true;
  const t = (v) => (v || "").trim();
  return ["host", "share", "username"].some((f) => t(saved[f]) !== t(form[f]));
}
