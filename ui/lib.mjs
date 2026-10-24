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

// ---------- translation ----------
// Texts live in locales/<lang>.json (flat "area.name" keys). A plural text is an
// object with one entry per category of the language; the count is the `n` param.

/** Plural category of `n` per language (CLDR rules, integers only). */
const PLURAL_RULES = {
  en: (n) => (n === 1 ? "one" : "other"),
  hr: (n) => {
    const d = n % 10;
    const dd = n % 100;
    if (d === 1 && dd !== 11) return "one";
    if (d >= 2 && d <= 4 && (dd < 12 || dd > 14)) return "few";
    return "other";
  },
};

/** The categories each language's plural texts must have. */
export const PLURAL_CATEGORIES = { en: ["one", "other"], hr: ["one", "few", "other"] };

export function pluralCategory(lang, n) {
  return (PLURAL_RULES[lang] || PLURAL_RULES.en)(n);
}

/**
 * A `t(key, params)` for `lang`: looks the key up in `dict`, then in `fallback`
 * (English), else returns the key. `{name}` placeholders take `params.name`;
 * numbers are formatted with `formatCount`.
 */
export function makeT(lang, dict, fallback) {
  return (key, params = {}) => {
    let value = dict[key];
    let valueLang = lang;
    if (value === undefined) {
      value = fallback[key];
      valueLang = "en";
    }
    if (value === undefined) return key;
    if (typeof value === "object") {
      value = value[pluralCategory(valueLang, params.n)] ?? value.other;
    }
    return value.replace(/\{(\w+)\}/g, (m, name) => {
      const v = params[name];
      if (v === undefined || v === null) return m;
      return typeof v === "number" ? formatCount(v) : String(v);
    });
  };
}

export function scanProgressText(p, t) {
  // Nothing is counted while the share is still being listed.
  if (!p || !p.seen) return t("scan.listing");
  const text = t("scan.progress", { files: t("scan.files", { n: p.seen }) });
  return p.failed ? t("scan.unreadable", { text, n: p.failed }) : text;
}

/** Text for the `scan-done` event payload. */
export function scanDoneText(d, t) {
  if (d.cancelled) return d.cancel_reason || t("scan.cancelled");
  if (d.error) return t("scan.failed", { error: d.error });
  return t("scan.done", {
    files: t("scan.files", { n: d.total }),
    read: d.read,
    failed: d.failed,
    removed: d.removed,
  });
}

const CONN_STATUSES = ["ok", "secondary", "unreachable", "auth"];

/** Text for a connection status code ("ok", "secondary", "unreachable", "auth"). */
export function connMessage(status, t) {
  return CONN_STATUSES.includes(status) ? t(`conn.${status}`) : "";
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
