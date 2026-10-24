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

/** Croatian plural: `forms` = [1, 2–4, 5+] (11–14 take the 5+ form). */
export function plural(n, forms) {
  const d = n % 10;
  const dd = n % 100;
  if (d === 1 && dd !== 11) return forms[0];
  if (d >= 2 && d <= 4 && (dd < 12 || dd > 14)) return forms[1];
  return forms[2];
}

const FILES = ["datoteka", "datoteke", "datoteka"];

export function scanProgressText(p) {
  // Nothing is counted while the share is still being listed.
  if (!p || !p.seen) return "Skeniram… popisujem datoteke";
  let text = `Skeniram… ${formatCount(p.seen)} ${plural(p.seen, FILES)}`;
  if (p.failed) text += ` (nečitljivo: ${formatCount(p.failed)})`;
  return text;
}

/** Text for the `scan-done` event payload. */
export function scanDoneText(d) {
  if (d.cancelled) return d.cancel_reason || "Skeniranje je prekinuto.";
  if (d.error) return `Skeniranje nije uspjelo: ${d.error}`;
  return (
    `Katalog osvježen: ${formatCount(d.total)} ${plural(d.total, FILES)}. ` +
    `Novo ili promijenjeno: ${formatCount(d.read)}, ` +
    `nečitljivo: ${formatCount(d.failed)}, uklonjeno: ${formatCount(d.removed)}.`
  );
}

// Same wording as the backend's `ConnStatus::message`.
const CONN_MESSAGES = {
  ok: "Spojeno",
  secondary: "Spojeno na rezervni server",
  unreachable: "Disk nije dostupan. Je li uključen WireGuard/VPN?",
  auth: "Pogrešno korisničko ime ili lozinka",
};

export function connMessage(status) {
  return CONN_MESSAGES[status] || "";
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
