// Pure helpers for the UI (no DOM, no Tauri), so `node --test ui/*.test.mjs` can
// check them.

const NBSP = " ";

/** 12340 -> "12 340" (no-break space between thousands). */
export function formatCount(n) {
  return String(Math.trunc(n)).replace(/\B(?=(\d{3})+(?!\d))/g, NBSP);
}

/** "Artist – Title (1991)" for the now-playing bar (no year if unknown); "—" when nothing plays. */
export function trackLabel(np) {
  if (!np) return "—";
  const parts = [np.artist, np.title].map((s) => (s || "").trim()).filter(Boolean);
  if (!parts.length) return "—";
  const label = parts.join(" – ");
  return np.year ? `${label} (${np.year})` : label;
}

/** An album of the artist list: "Name (1999)"; the year is left out when unknown. */
export function albumLabel(album, t) {
  const name = album.name || t("albums.unknown");
  // A string, so the year is not grouped like a count ("1 999").
  return album.year ? t("albums.year", { name, year: String(album.year) }) : name;
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

/**
 * Calls `fn` with the latest arguments once calls have stopped for `ms`.
 * `.flush()` runs a pending call at once.
 */
export function debounce(fn, ms) {
  let timer = null;
  let pending = null;
  const run = () => {
    const args = pending;
    clearTimeout(timer);
    timer = null;
    pending = null;
    if (args) fn(...args);
  };
  const debounced = (...args) => {
    pending = args;
    clearTimeout(timer);
    timer = setTimeout(run, ms);
  };
  debounced.flush = run;
  return debounced;
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
  const done = t("scan.done", {
    files: t("scan.files", { n: d.total }),
    read: d.read,
    failed: d.failed,
    removed: d.removed,
  });
  // Folders the share would not list: their tracks were kept, not removed.
  return d.skipped_dirs ? `${done} ${t("scan.skipped_dirs", { n: d.skipped_dirs })}` : done;
}

const CONN_STATUSES = ["ok", "secondary", "unreachable", "auth"];

/** Text for a connection status code ("ok", "secondary", "unreachable", "auth"). */
export function connMessage(status, t) {
  return CONN_STATUSES.includes(status) ? t(`conn.${status}`) : "";
}

const UNKNOWN_KEY = "_none";

/**
 * Genres that genre `fromKey` can be merged into, matching `query`: first the genres
 * that have tracks (without `fromKey` and Unknown), then, only while searching, the
 * other known genres. Each group is sorted.
 */
export function mergeChoices(knownNames, genres, fromKey, query) {
  const q = foldForSearch(query);
  const matches = (n) => !q || foldForSearch(n).includes(q);
  const byName = (a, b) => a.localeCompare(b, "en", { sensitivity: "base" });
  const present = genres.filter((g) => g.key !== UNKNOWN_KEY).map((g) => g.display);
  const from = genres.find((g) => g.key === fromKey);
  const withTracks = present.filter((n) => (!from || n !== from.display) && matches(n)).sort(byName);
  if (!q) return withTracks;
  const taken = new Set(present);
  const others = knownNames.filter((n) => !taken.has(n) && matches(n)).sort(byName);
  return [...withTracks, ...others];
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

// ---------- unreadable files ----------

/** Failure codes the backend stores (see `crates/core/src/failure.rs`). */
export const FAIL_CODES = ["not_found", "timeout", "unreachable", "decode", "unsupported", "tag", "io"];

/** Why a file is unreadable, in words; "Unknown reason" when not recorded. */
export function reasonText(code, t) {
  return t(`unreadable.reason_${FAIL_CODES.includes(code) ? code : "unknown"}`);
}

/** The tab's label, with the count when there are unreadable files. */
export function unreadableTabLabel(n, t) {
  return n ? t("tabs.unreadable_n", { n }) : t("tabs.unreadable");
}

/** "Copy all": one line per file, "url<TAB>reason (message)" (the encoded URL). */
export function copyAllText(rows, t) {
  return rows
    .map((r) => {
      const reason = reasonText(r.code, t);
      return `${r.url}\t${r.message ? `${reason} (${r.message})` : reason}`;
    })
    .join("\n");
}

/** Unix seconds as a short local date and time in `lang`; "" when unknown. */
export function formatWhen(secs, lang) {
  if (!secs) return "";
  return new Date(secs * 1000).toLocaleString(lang, { dateStyle: "medium", timeStyle: "short" });
}

// ---------- now playing: time ----------

const HOUR_MS = 3_600_000;

/** `ms` as "m:ss", or "h:mm:ss" from an hour on (or always, with `hours`). */
export function formatTime(ms, hours = false) {
  const total = Math.max(0, Math.floor((Number(ms) || 0) / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, "0");
  return hours || h ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

/** "1:23 / 4:56"; just the elapsed time when the length is unknown. */
export function positionText(positionMs, durationMs) {
  if (!durationMs) return formatTime(positionMs);
  const hours = durationMs >= HOUR_MS;
  return `${formatTime(Math.min(positionMs, durationMs), hours)} / ${formatTime(durationMs, hours)}`;
}

/** How far into the track (0..1); null when its length is unknown. */
export function progressFraction(positionMs, durationMs) {
  if (!durationMs) return null;
  return Math.min(1, Math.max(0, (positionMs || 0) / durationMs));
}

// ---------- playlist ----------

/**
 * The backend `QueueFilter` for the playlist's filter controls; null when nothing
 * is set. A year range open on one side runs from 0 or to 9999.
 */
export function queueFilter({ text, genres, from, to, unknownYear } = {}) {
  const f = {};
  const q = (text || "").trim();
  if (q) f.text = q;
  if (genres && genres.length) f.genres = [...genres];
  const hasFrom = from !== null && from !== undefined;
  const hasTo = to !== null && to !== undefined;
  if (hasFrom || hasTo) {
    const lo = hasFrom ? from : 0;
    const hi = hasTo ? to : 9999;
    f.years = [Math.min(lo, hi), Math.max(lo, hi)];
  }
  if (unknownYear) f.unknown_year = true;
  return Object.keys(f).length ? f : null;
}

/** "Trance + Chillout 2026-10-25.m3u8" (local date). */
export function exportFileName(stem, date) {
  const pad = (n) => String(n).padStart(2, "0");
  const day = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
  return `${stem || "Playlist"} ${day}.m3u8`;
}

/**
 * The rows of a long list to put in the page: those in view plus `overscan` on
 * each side. `listTop` is the list's top relative to the top of the view (negative
 * once scrolled past). `end` is exclusive.
 */
export function windowRange(listTop, viewHeight, rowHeight, total, overscan) {
  const top = Math.max(0, -listTop);
  const bottom = Math.max(0, viewHeight - listTop);
  const start = Math.max(0, Math.floor(top / rowHeight) - overscan);
  const end = Math.min(total, Math.ceil(bottom / rowHeight) + overscan);
  return { start: Math.min(start, end), end };
}

/** Offsets of the pages (of `size` rows) that cover rows `start..end`. */
export function pageOffsets(start, end, size) {
  const out = [];
  for (let p = Math.floor(start / size) * size; p < end; p += size) out.push(p);
  return out;
}

/** The genre filter's label: all genres, the one chosen, or how many. */
export function genresSummary(selected, genres, t) {
  if (!selected.length) return t("playlist.genres_all");
  if (selected.length === 1) {
    const g = genres.find((x) => x.key === selected[0]);
    return g ? g.display : selected[0];
  }
  return t("playlist.genres_n", { n: selected.length });
}

// ---------- artists ----------

/** Whether two lists hold the same items in the same order. */
export function sameItems(a, b) {
  return Boolean(a && b) && a.length === b.length && a.every((x, i) => x === b[i]);
}

/** The open artists that are still in `names`. */
export function keepExpanded(expanded, names) {
  const present = new Set(names);
  return new Set([...expanded].filter((n) => present.has(n)));
}

/**
 * How to turn rows with keys `oldKeys` into rows with `newKeys` touching as few as
 * possible: the positions to replace, how many to add at the end, how many to
 * remove from the end.
 */
export function patchPlan(oldKeys, newKeys) {
  const common = Math.min(oldKeys.length, newKeys.length);
  const replace = [];
  for (let i = 0; i < common; i += 1) if (oldKeys[i] !== newKeys[i]) replace.push(i);
  return {
    replace,
    add: Math.max(0, newKeys.length - oldKeys.length),
    remove: Math.max(0, oldKeys.length - newKeys.length),
  };
}

/** Wait before the `n`th retry of a failed fetch (1-based); null: give up. */
export function retryDelay(n) {
  return n >= 1 && n <= 5 ? 500 * 2 ** (n - 1) : null;
}
