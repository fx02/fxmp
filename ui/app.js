// UI wiring: Tauri commands/events <-> DOM. Pure helpers live in lib.mjs.
import {
  formatCount,
  trackLabel,
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
  makeT,
  albumLabel,
  reasonText,
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
} from "./lib.mjs";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ARTIST_LIMIT = 300;
const UNKNOWN_KEY = "_none";
const BAN_CONFIRM_MS = 3000;
const SUGGESTED_SECONDARY = { host: "10.111.0.2", share: "F", root: "MP3", username: "", domain: null };

const $ = (id) => document.getElementById(id);

// Translation: `t(key, params)` in the current language (see lib.mjs, locales/).
let t = (key) => key;

const state = {
  tab: null,
  settings: null,
  configured: false,
  nowPlaying: null,
  genres: [],
  knownGenres: null,
  selected: new Set(),
  artists: null,
  artistsStale: true,
  scanning: false,
  rescanIfCancelled: false,
  // Last connection-status payload and scan texts, re-worded on a language change.
  status: null,
  statusChecking: false,
  scanText: null,
  // "Unreadable" tab: the count on the tab and the rows (loaded when shown).
  unreadableCount: 0,
  unreadable: null,
  // The last `position` event.
  position: null,
  // Lookup sites: [{ id, name }].
  lookupServices: [],
  // Artists tab: open artists (their albums shown) and open albums (their tracks
  // shown, keyed artist + NUL + album), kept across re-renders.
  expanded: new Set(),
  openAlbums: new Set(),
  albumCache: new Map(),
  trackCache: new Map(),
};

// ---------- helpers ----------

let toastTimer = null;
function toast(msg) {
  const el = $("toast");
  el.textContent = msg;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), 4000);
}

/** Runs a command; on failure shows the error and returns undefined. */
async function run(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e));
    return undefined;
  }
}

/** Puts `text` on the clipboard; resolves to whether it worked. */
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Fallback for a webview without the async clipboard.
    const area = el("textarea", { value: text });
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.append(area);
    area.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    area.remove();
    return ok;
  }
}

async function copyAndTell(text) {
  toast((await copyText(text)) ? t("unreadable.copied") : t("unreadable.copy_failed"));
}

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  Object.assign(node, props);
  node.append(...children);
  return node;
}

// ---------- tabs ----------

function showTab(name) {
  state.tab = name;
  for (const b of document.querySelectorAll(".tab")) {
    const active = b.dataset.tab === name;
    b.classList.toggle("active", active);
    b.setAttribute("aria-selected", String(active));
  }
  for (const p of document.querySelectorAll(".panel")) p.hidden = p.id !== `tab-${name}`;
  $("menu").hidden = true;
  if (name === "artists") loadArtists();
  if (name === "playlist") showPlaylist();
  if (name === "unreadable") loadUnreadable();
  if (name === "settings") {
    loadDevices();
    loadBanned();
  }
}

// ---------- now playing ----------

function lookupServiceName(id) {
  const s = state.lookupServices.find((x) => x.id === id) || state.lookupServices[0];
  return s ? s.name : "";
}

function renderNowPlaying(np) {
  const changed = (np && np.id) !== (state.nowPlaying && state.nowPlaying.id);
  state.nowPlaying = np;
  const label = $("np-label");
  label.textContent = trackLabel(np);
  label.disabled = !np;
  const service = lookupServiceName(state.settings && state.settings.lookup_service);
  label.title = np
    ? [trackLabel(np), np.album, t("player.lookup_hint", { service })].filter(Boolean).join(" · ")
    : "";
  label.classList.toggle("paused", Boolean(np && np.paused));
  renderPosition();
  const chip = $("np-genre");
  chip.textContent = (np && np.genre_display) || "";
  chip.title = chip.textContent;
  chip.hidden = !chip.textContent;
  $("btn-pause").title = np && np.paused ? t("player.resume") : t("player.pause");
  $("btn-ban").disabled = !np;
  // A pending ban confirmation was for the previous track.
  if (changed) resetBan();
  // The playlist's cursor moved (or the pause changed nothing there).
  if (changed) playlistMoved();
}

/** Elapsed / total time and the progress bar, from the last `position` event. */
function renderPosition() {
  const np = state.nowPlaying;
  $("np-progress").hidden = !np;
  if (!np) return;
  const p = state.position && state.position.id === np.id ? state.position : null;
  const ms = p ? p.position_ms : 0;
  const total = (p && p.duration_ms) || np.duration_ms || null;
  $("np-time").textContent = positionText(ms, total);
  $("np-time").title = total ? "" : t("player.no_length");
  const f = progressFraction(ms, total);
  $("np-fill").style.width = `${((f === null ? 0 : f) * 100).toFixed(2)}%`;
}

function onPosition(p) {
  state.position = p;
  renderPosition();
}

let banTimer = null;
function resetBan() {
  clearTimeout(banTimer);
  const b = $("btn-ban");
  b.classList.remove("confirm");
  b.textContent = "🚫";
}

async function onBan() {
  const b = $("btn-ban");
  if (!b.classList.contains("confirm")) {
    b.classList.add("confirm");
    b.textContent = t("player.ban_confirm");
    banTimer = setTimeout(resetBan, BAN_CONFIRM_MS);
    return;
  }
  const label = trackLabel(state.nowPlaying);
  resetBan();
  try {
    await invoke("ban_current");
    toast(t("player.banned", { track: label }));
    if (state.tab === "settings") loadBanned();
  } catch (e) {
    toast(String(e));
  }
}

const sendVolume = debounce((v) => run("set_volume", { v }), 60);

// ---------- track context menu ----------
// The app's own menu (not WebKit's) on every track row and the now-playing bar.
// Items: `{ label, run }`, `{ label, sub: [items] }` (opens in place) or `"-"`.

function closeCtx() {
  const box = $("ctx");
  box.hidden = true;
  box.replaceChildren();
}

function ctxButton(label, onClick, className = "") {
  const b = el("button", { type: "button", textContent: label, className });
  b.setAttribute("role", "menuitem");
  b.addEventListener("click", (e) => {
    e.stopPropagation();
    onClick(b);
  });
  return b;
}

function ctxItems(items, className = "") {
  const out = [];
  for (const item of items) {
    if (item === "-") {
      out.push(el("hr"));
    } else if (item.sub) {
      const sub = el("div", { className: "submenu", hidden: true }, ...ctxItems(item.sub, "indent"));
      const head = ctxButton(`${item.label} ▸`, () => {
        sub.hidden = !sub.hidden;
        head.textContent = `${item.label} ${sub.hidden ? "▸" : "▾"}`;
        placeCtx();
      });
      out.push(head, sub);
    } else {
      out.push(
        ctxButton(
          item.label,
          () => {
            closeCtx();
            item.run();
          },
          className,
        ),
      );
    }
  }
  return out;
}

let ctxAt = { x: 0, y: 0 };
function placeCtx() {
  const box = $("ctx");
  box.style.left = `${Math.max(4, Math.min(ctxAt.x, window.innerWidth - box.offsetWidth - 4))}px`;
  box.style.top = `${Math.max(4, Math.min(ctxAt.y, window.innerHeight - box.offsetHeight - 4))}px`;
}

function openCtx(items, x, y) {
  $("menu").hidden = true;
  const box = $("ctx");
  box.replaceChildren(...ctxItems(items));
  box.hidden = false;
  ctxAt = { x, y };
  placeCtx();
}

/** "Artist – Title (Year)" on the clipboard, with a toast. */
async function copySong(track) {
  const text = trackLabel(track);
  toast((await copyText(text)) ? t("menu.copied", { text }) : t("unreadable.copy_failed"));
}

async function addToPlaylist(ids, at) {
  const n = await run("playlist_add", { ids, at });
  if (n !== undefined) toast(t("playlist.added", { n }));
}

/**
 * The menu of a track `{ id, artist, title, year }`: copy, look up, and (with an
 * id, unless `noAdd`) Play next / Add to end, then `extra` items.
 */
function trackMenu(track, { noAdd = false, extra = [] } = {}) {
  const artist = track.artist || "";
  const title = track.title || "";
  const items = [{ label: t("menu.copy_song"), run: () => copySong(track) }];
  items.push({
    label: t("menu.lookup_on"),
    sub: state.lookupServices.map((s) => ({
      label: s.name,
      run: () => run("lookup_track", { service: s.id, artist, title }),
    })),
  });
  if (artist.trim()) items.push({ label: t("menu.lookup_artist"), run: () => run("lookup_artist", { artist }) });
  if (!noAdd && track.id !== undefined) {
    items.push(
      "-",
      { label: t("menu.play_next"), run: () => addToPlaylist([track.id], "next") },
      { label: t("menu.add_to_end"), run: () => addToPlaylist([track.id], "end") },
    );
  }
  if (extra.length) items.push("-", ...extra);
  return items;
}

/** Right-click / ⋯ on the now-playing bar. */
function nowPlayingMenu(x, y) {
  const np = state.nowPlaying;
  if (!np) return;
  openCtx(
    trackMenu(np, {
      noAdd: true,
      extra: [{ label: t("menu.ban"), run: () => banFromMenu(np) }],
    }),
    x,
    y,
  );
}

async function banFromMenu(np) {
  const track = trackLabel(np);
  const ok = await confirmDialog({ title: t("menu.ban_title", { track }), text: t("menu.ban_text"), okLabel: t("menu.ban") });
  if (!ok) return;
  // The track may have changed while the dialog was open: ban only that one.
  if (!state.nowPlaying || state.nowPlaying.id !== np.id) return;
  try {
    await invoke("ban_current");
    toast(t("player.banned", { track }));
    if (state.tab === "settings") loadBanned();
  } catch (e) {
    toast(String(e));
  }
}

// ---------- playlist ----------
// A windowed list: only the rows in view (plus a margin) are in the page, fetched
// in pages of PL_PAGE rows. The backend's playlist is the only copy of the list.

const PL_ROW_H = 28;
const PL_PAGE = 200;
const PL_OVERSCAN = 12;

const pl = {
  // The last page's header: title, count, total, current_row, version, ….
  meta: null,
  // Row number (in the filtered list) -> row.
  rows: new Map(),
  // Page offsets asked for since the last refresh.
  fetched: new Set(),
  // Bumped by every refresh; answers of older refreshes are dropped.
  gen: 0,
  // Whether the rows were replaced since the last refresh.
  fresh: false,
  version: "",
  filter: null,
  facets: null,
  genres: [],
  // Scroll to the current track once the next page arrives.
  scrollToCurrent: false,
  stale: true,
  // Bumped whenever `rows` change; with the window it says whether the drawn
  // rows are still right (rows are not rebuilt under a click for nothing).
  stamp: 0,
  drawn: "",
  // The playlist version the facets were read for ("" = read them again).
  facetsFor: "",
  // Failed page fetches in a row (they are retried with a back-off).
  retries: 0,
};

function plWindow(total) {
  const main = document.querySelector("main");
  const top = $("pl-list").getBoundingClientRect().top - main.getBoundingClientRect().top;
  return windowRange(top, main.clientHeight, PL_ROW_H, total, PL_OVERSCAN);
}

function rowVisible(row) {
  if (row === null || row === undefined) return false;
  const { start, end } = plWindow(pl.meta ? pl.meta.total : 0);
  return row >= start + PL_OVERSCAN && row < end - PL_OVERSCAN;
}

function showPlaylist() {
  pl.scrollToCurrent = pl.stale || pl.scrollToCurrent;
  pl.stale = false;
  // The facets are read again once a page shows the playlist changed.
  refreshPlaylist();
}

/** Fetches the rows in view again (the list or the cursor changed). */
function refreshPlaylist() {
  pl.gen += 1;
  pl.fresh = false;
  pl.fetched = new Set();
  ensurePages();
}

const refreshPlaylistSoon = debounce(refreshPlaylist, 120);

/** The cursor moved: refresh, following the current track if it was in view. */
function playlistMoved() {
  if (state.tab !== "playlist") {
    pl.stale = true;
    return;
  }
  if (pl.meta && rowVisible(pl.meta.current_row)) pl.scrollToCurrent = true;
  refreshPlaylistSoon();
}

function onPlaylistChanged() {
  if (state.tab !== "playlist") {
    pl.stale = true;
    return;
  }
  refreshPlaylistSoon();
}

function ensurePages() {
  const total = pl.meta ? pl.meta.total : PL_PAGE;
  const { start, end } = plWindow(total);
  const offsets = pageOffsets(start, Math.max(end, start + 1), PL_PAGE);
  for (const offset of offsets) if (!pl.fetched.has(offset)) fetchPage(offset);
}

async function fetchPage(offset) {
  const gen = pl.gen;
  pl.fetched.add(offset);
  const page = await run("get_playlist", { offset, limit: PL_PAGE, filter: pl.filter });
  if (gen !== pl.gen) return;
  if (!page) {
    // Asked again after a pause (a few times), unless a refresh comes first.
    pl.fetched.delete(offset);
    pl.retries += 1;
    const wait = retryDelay(pl.retries);
    if (wait !== null) setTimeout(() => gen === pl.gen && ensurePages(), wait);
    return;
  }
  pl.retries = 0;
  const version = page.version.join(".");
  if (!pl.fresh || version !== pl.version) {
    // First answer since the refresh (or the list changed): start from it.
    pl.rows = new Map();
    pl.fetched = new Set([offset]);
    pl.fresh = true;
    pl.version = version;
  }
  page.rows.forEach((row, i) => pl.rows.set(offset + i, row));
  pl.stamp += 1;
  pl.meta = page;
  if (pl.facetsFor !== version) {
    pl.facetsFor = version;
    if (pl.facets) loadFacetsSoon();
    else loadFacets();
  }
  renderPlaylistHead();
  if (pl.scrollToCurrent) {
    pl.scrollToCurrent = false;
    if (scrollToCurrent()) return;
  }
  renderPlaylistRows();
}

/** Scrolls the current track into view (a third from the top); false if none. */
function scrollToCurrent() {
  const row = pl.meta && pl.meta.current_row;
  if (row === null || row === undefined) return false;
  const main = document.querySelector("main");
  const listTop = $("pl-list").getBoundingClientRect().top - main.getBoundingClientRect().top + main.scrollTop;
  main.scrollTop = Math.max(0, listTop + row * PL_ROW_H - main.clientHeight / 3);
  renderPlaylistRows();
  return true;
}

function renderPlaylistHead() {
  const m = pl.meta;
  if (!m) return;
  $("pl-title").textContent = m.title;
  $("pl-title").title = m.title;
  $("pl-count").textContent = pl.filter
    ? t("playlist.count_filtered", { shown: m.total, n: m.count })
    : t("playlist.count", { n: m.count });
  const empty = $("pl-empty");
  empty.hidden = m.total > 0;
  empty.textContent = m.count ? t("playlist.no_match") : t("playlist.empty");
  $("pl-play-filtered").disabled = !pl.filter || !m.total;
  $("pl-export").disabled = !m.total;
}

function playlistRow(row, pos) {
  const last = pl.meta ? pl.meta.count - 1 : 0;
  const actions = el(
    "span",
    { className: "row-actions" },
    el("button", { type: "button", className: "up", textContent: "↑", title: t("playlist.move_up"), disabled: row.index === 0 }),
    el("button", { type: "button", className: "down", textContent: "↓", title: t("playlist.move_down"), disabled: row.index >= last }),
    el("button", { type: "button", className: "remove", textContent: "×", title: t("playlist.remove") }),
  );
  const label = trackLabel(row);
  const node = el(
    "div",
    { className: `pl-row ${row.state}` },
    el("span", { className: "name", textContent: label, title: label }),
    row.manual ? el("span", { className: "badge", textContent: "+", title: t("playlist.manual_row") }) : "",
    el("span", { className: "dur", textContent: row.duration_ms ? formatTime(row.duration_ms) : "" }),
    actions,
  );
  node.setAttribute("role", "listitem");
  node.style.top = `${pos * PL_ROW_H}px`;
  node.dataset.pos = String(pos);
  return node;
}

function renderPlaylistRows() {
  const list = $("pl-list");
  const total = pl.meta ? pl.meta.total : 0;
  list.style.height = `${total * PL_ROW_H}px`;
  const { start, end } = plWindow(total);
  const drawn = `${start}:${end}:${pl.stamp}:${document.documentElement.lang}`;
  if (drawn !== pl.drawn) {
    pl.drawn = drawn;
    const nodes = [];
    for (let pos = start; pos < end; pos += 1) {
      const row = pl.rows.get(pos);
      if (row) nodes.push(playlistRow(row, pos));
    }
    list.replaceChildren(...nodes);
  }
  ensurePages();
}

let plFrame = 0;
function onMainScroll() {
  if (state.tab !== "playlist" || plFrame) return;
  plFrame = requestAnimationFrame(() => {
    plFrame = 0;
    renderPlaylistRows();
  });
}

function plRowAt(target) {
  const node = target.closest(".pl-row");
  return node ? pl.rows.get(Number(node.dataset.pos)) : null;
}

async function onPlaylistClick(e) {
  const row = plRowAt(e.target);
  if (!row) return;
  const button = e.target.closest("button");
  if (!button) {
    run("playlist_jump", { index: row.index, id: row.id });
  } else if (button.classList.contains("remove")) {
    run("playlist_remove", { index: row.index, id: row.id });
  } else if (button.classList.contains("up") || button.classList.contains("down")) {
    const to = row.index + (button.classList.contains("up") ? -1 : 1);
    run("playlist_move", { from: row.index, to, id: row.id });
  }
}

function onPlaylistMenu(e) {
  const row = plRowAt(e.target);
  if (!row) return;
  e.preventDefault();
  openCtx(
    trackMenu(row, {
      extra: [{ label: t("playlist.remove"), run: () => run("playlist_remove", { index: row.index, id: row.id }) }],
    }),
    e.clientX,
    e.clientY,
  );
}

// Filters: they narrow what is shown (and exported), never the play order.

const filterTyped = debounce(() => applyFilter(), 250);

async function loadFacets() {
  const facets = await run("playlist_facets");
  if (!facets) return;
  pl.facets = facets;
  const keys = new Set(facets.genres.map((g) => g.key));
  const kept = pl.genres.filter((k) => keys.has(k));
  renderFacets();
  if (kept.length !== pl.genres.length) {
    pl.genres = kept;
    applyFilter();
  }
}

const loadFacetsSoon = debounce(loadFacets, 400);

function genreFacetView() {
  return (pl.facets ? pl.facets.genres : []).map((g) =>
    g.key === UNKNOWN_KEY ? { ...g, display: t("genres.unknown") } : g,
  );
}

function renderFacets() {
  const genres = genreFacetView();
  const chosen = new Set(pl.genres);
  $("pl-genre-list").replaceChildren(
    ...genres.map((g) => {
      const box = el("input", { type: "checkbox", checked: chosen.has(g.key) });
      box.dataset.key = g.key;
      return el("label", {}, box, el("span", { className: "name", textContent: g.display }), el("span", { className: "count", textContent: formatCount(g.count) }));
    }),
  );
  $("pl-genres-label").textContent = genresSummary(pl.genres, genres, t);
  const years = pl.facets ? pl.facets.years.map((y) => y.year).sort((a, b) => a - b) : [];
  for (const [id, key] of [["pl-year-from", "playlist.year_from"], ["pl-year-to", "playlist.year_to"]]) {
    const sel = $(id);
    const was = sel.value;
    sel.replaceChildren(
      el("option", { value: "", textContent: t(key) }),
      ...years.map((y) => el("option", { value: String(y), textContent: String(y) })),
    );
    sel.value = years.includes(Number(was)) ? was : "";
  }
}

function readFilter() {
  const year = (id) => ($(id).value ? Number($(id).value) : null);
  return queueFilter({
    text: $("pl-filter").value,
    genres: pl.genres,
    from: year("pl-year-from"),
    to: year("pl-year-to"),
    unknownYear: $("pl-unknown-year").checked,
  });
}

function applyFilter() {
  pl.filter = readFilter();
  $("pl-genres-label").textContent = genresSummary(pl.genres, genreFacetView(), t);
  pl.meta = null;
  pl.rows = new Map();
  pl.stamp += 1;
  $("pl-list").replaceChildren();
  pl.scrollToCurrent = true;
  refreshPlaylist();
}

function clearFilters() {
  $("pl-filter").value = "";
  $("pl-year-from").value = "";
  $("pl-year-to").value = "";
  $("pl-unknown-year").checked = false;
  pl.genres = [];
  renderFacets();
  applyFilter();
}

async function onPlayFiltered() {
  if (!pl.filter) return;
  try {
    await invoke("playlist_play_filtered", { filter: pl.filter });
  } catch (e) {
    toast(String(e));
    return;
  }
  // The new playlist is the filtered tracks: show all of it.
  clearFilters();
}

async function onNewPlaylist() {
  const ok = await confirmDialog({
    title: t("playlist.new_title"),
    text: t("playlist.new_text"),
    okLabel: t("playlist.new_button"),
    danger: false,
  });
  if (ok) run("playlist_new");
}

async function onExport() {
  // What was typed last counts, even within the filter's debounce.
  filterTyped.flush();
  const hint = await run("playlist_export_hint");
  if (!hint) return;
  let filtered = null;
  if (pl.filter) {
    const page = await run("get_playlist", { offset: 0, limit: 1, filter: pl.filter });
    if (!page) return;
    filtered = page.total;
    if (!filtered) {
      toast(t("err.filter_empty"));
      return;
    }
  }
  const answer = await confirmDialog({
    title: t("playlist.export_title"),
    text: t("playlist.export_text", { folder: hint.music_folder }),
    okLabel: t("playlist.export_save"),
    danger: false,
    check: filtered === null ? null : { label: t("playlist.export_only_filtered", { n: filtered }), note: () => "", checked: true },
  });
  if (!answer) return;
  const filter = answer.checked ? pl.filter : null;
  try {
    const done = await invoke("playlist_export_save", {
      file_name: exportFileName(hint.file_stem, new Date()),
      filter,
    });
    if (done) toast(t("playlist.exported", { n: done.count, path: done.path }));
  } catch (e) {
    toast(String(e));
  }
}

// ---------- connection status ----------

function renderStatus(payload, checking = false) {
  state.status = payload;
  state.statusChecking = checking;
  const dot = $("conn-dot");
  dot.className = "dot" + (payload ? ` ${payload.status}` : "");
  $("conn-text").textContent = payload
    ? connMessage(payload.status, t)
    : state.configured || checking
      ? t("conn.checking")
      : t("conn.not_set_up");
}

// ---------- scan ----------

let scanNoteTimer = null;
function renderScanProgress(progress) {
  const started = !state.scanning;
  state.scanning = true;
  if (started) renderGenres();
  const text = scanProgressText(progress, t);
  state.scanText = { progress };
  clearTimeout(scanNoteTimer);
  $("genre-scan").textContent = text;
  $("genre-scan").hidden = false;
  $("scan-status").textContent = text;
  $("btn-scan").disabled = true;
  $("btn-scan-cancel").hidden = false;
}

function renderScanDone(done) {
  state.scanning = false;
  const text = scanDoneText(done, t);
  state.scanText = { done };
  $("genre-scan").textContent = text;
  $("scan-status").textContent = text;
  $("btn-scan").disabled = false;
  $("btn-scan-cancel").hidden = true;
  clearTimeout(scanNoteTimer);
  scanNoteTimer = setTimeout(() => ($("genre-scan").hidden = true), 8000);
  renderGenres();
  const rescan = state.rescanIfCancelled && done.cancelled && Boolean(done.cancel_reason);
  state.rescanIfCancelled = false;
  if (rescan) startScan();
}

function renderScanIdle() {
  state.scanning = false;
  $("genre-scan").hidden = true;
  $("btn-scan").disabled = false;
  $("btn-scan-cancel").hidden = true;
  renderGenres();
}

/** Re-reads the scan state from the backend (after a failed start, say). */
async function syncScanStatus() {
  const scan = await run("get_scan_status");
  if (!scan) return;
  if (scan.scanning) renderScanProgress(scan.progress);
  else renderScanIdle();
}

// Shown before the request: the scan's own events (`scan-progress`, then
// `scan-done`) may arrive before the reply and must have the last word.
async function startScan() {
  renderScanProgress(null);
  try {
    await invoke("start_scan");
  } catch (e) {
    toast(String(e));
    syncScanStatus();
  }
}

// ---------- genres ----------

async function loadGenres() {
  const genres = await run("get_genres");
  if (!genres) return;
  state.genres = genres;
  const keys = new Set(genres.map((g) => g.key));
  for (const k of state.selected) if (!keys.has(k)) state.selected.delete(k);
  renderGenres();
}

const refreshGenres = debounce(loadGenres, 500);

/** The genres as shown: Unknown in the current language. */
function genreView() {
  return state.genres.map((g) => (g.key === UNKNOWN_KEY ? { ...g, display: t("genres.unknown") } : g));
}

function renderGenres() {
  const shown = filterGenres(genreView(), $("genre-filter").value);
  const frag = document.createDocumentFragment();
  for (const g of shown) {
    const li = el(
      "li",
      {},
      el("input", { type: "checkbox", checked: state.selected.has(g.key), title: t("genres.select") }),
      el("span", { className: "name", textContent: g.display, title: t("genres.play", { name: g.display }) }),
      el("span", { className: "count", textContent: formatCount(g.count) }),
    );
    // Unknown cannot be merged: it holds the tracks with no known genre.
    if (g.key !== UNKNOWN_KEY) li.append(el("button", { className: "more", textContent: "⋯", title: t("genres.merge_into") }));
    li.dataset.key = g.key;
    frag.append(li);
  }
  $("genre-list").replaceChildren(frag);

  const empty = $("genre-empty");
  if (!state.genres.length) {
    empty.textContent = state.scanning
      ? t("genres.empty_scanning")
      : state.configured
        ? t("genres.empty_configured")
        : t("genres.empty_unconfigured");
  } else if (!shown.length) {
    empty.textContent = t("genres.no_match");
  }
  empty.hidden = Boolean(shown.length);
  // An empty library offers the refresh right here.
  $("btn-empty-scan").hidden = Boolean(state.genres.length) || !state.configured || state.scanning;
  $("btn-play-all").disabled = !state.genres.length;
  renderSelection();
}

function renderSelection() {
  const n = state.selected.size;
  const b = $("btn-play-selected");
  b.disabled = n === 0;
  b.textContent = n ? t("genres.play_selected_n", { n }) : t("genres.play_selected");
}

function playGenres(keys) {
  run("play_genres", { keys });
}

function genreOf(key) {
  return state.genres.find((g) => g.key === key);
}

function openMenu(key, x, y) {
  const menu = $("menu");
  menu.dataset.key = key;
  menu.hidden = false;
  const w = menu.offsetWidth;
  const h = menu.offsetHeight;
  menu.style.left = `${Math.min(x, window.innerWidth - w - 4)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - h - 4)}px`;
}

/** The known genre names (loaded once; the list is fixed in the app). */
async function knownGenres() {
  if (!state.knownGenres) state.knownGenres = (await run("known_genres")) || null;
  return state.knownGenres || [];
}

/** Asks which genre to merge `from` into; resolves to its name, or null if cancelled. */
function askMergeTarget(from, known) {
  return new Promise((resolve) => {
    const overlay = $("dialog");
    const form = $("dialog-form");
    const filter = $("dialog-filter");
    const choices = $("dialog-choices");
    const fill = () => {
      const names = mergeChoices(known, state.genres, from.key, filter.value);
      choices.replaceChildren(...names.map((n) => el("option", { value: n, textContent: n })));
      if (names.length) choices.selectedIndex = 0;
    };
    $("dialog-title").textContent = t("merge.title", { name: from.display });
    filter.value = "";
    fill();
    overlay.hidden = false;
    filter.focus();
    const finish = (value) => {
      overlay.hidden = true;
      form.onsubmit = null;
      filter.oninput = null;
      choices.ondblclick = null;
      $("dialog-cancel").onclick = null;
      overlay.onkeydown = null;
      resolve(value);
    };
    filter.oninput = fill;
    form.onsubmit = (e) => {
      e.preventDefault();
      finish(choices.value || null);
    };
    choices.ondblclick = () => finish(choices.value || null);
    $("dialog-cancel").onclick = () => finish(null);
    overlay.onkeydown = (e) => {
      if (e.key === "Escape") finish(null);
      // Arrow keys in the filter move through the list.
      else if (e.target === filter && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
        e.preventDefault();
        const i = choices.selectedIndex + (e.key === "ArrowDown" ? 1 : -1);
        if (i >= 0 && i < choices.options.length) choices.selectedIndex = i;
      }
    };
  });
}

async function onMenuAction(action) {
  const menu = $("menu");
  menu.hidden = true;
  const g = genreOf(menu.dataset.key);
  if (!g || action !== "merge") return;
  const into = await askMergeTarget(g, await knownGenres());
  if (!into) return;
  try {
    await invoke("merge_genre", { from_key: g.key, into_display: into });
    state.selected.delete(g.key);
    toast(t("merge.done", { from: g.display, into }));
  } catch (e) {
    toast(String(e));
  }
}

// ---------- artists ----------
// Re-renders (a scan reports new genres every few seconds) keep the open artists
// and albums, the search text and what is in view. They draw from the albums and
// tracks already loaded; then the open ones are read again and only rows that
// changed are replaced, so nothing flickers.

const albumKey = (artist, album) => `${artist}\u0000${album}`;
const rowKey = (x) => JSON.stringify(x);

/**
 * Runs `change` (a DOM change in the list) keeping the first artist in view where
 * it was on screen: rows added or removed above it do not move the view.
 */
function keepAnchor(change) {
  const main = document.querySelector("main");
  const top = main.getBoundingClientRect().top;
  const artists = () => [...$("artist-list").children].filter((li) => li.classList.contains("artist"));
  const anchor = artists().find((li) => li.getBoundingClientRect().bottom > top);
  const name = anchor ? anchor.dataset.artist : null;
  const before = anchor ? anchor.getBoundingClientRect().top : 0;
  const scroll = main.scrollTop;
  change();
  const now = name === null ? null : artists().find((li) => li.dataset.artist === name);
  if (now) main.scrollTop += now.getBoundingClientRect().top - before;
  else main.scrollTop = scroll;
}

async function loadArtists() {
  if (state.artists && !state.artistsStale) return;
  state.artistsStale = false;
  const artists = await run("list_artists");
  if (!artists) return;
  if (!sameItems(state.artists, artists)) {
    state.artists = artists;
    state.expanded = keepExpanded(state.expanded, artists);
    keepAnchor(renderArtists);
  }
  // The library changed: open artists and albums may have more (or fewer) rows.
  for (const li of document.querySelectorAll("#artist-list > li.artist.open")) refreshAlbums(li);
}

const reloadArtists = debounce(loadArtists, 1000);

/**
 * Draws the artist list from what is loaded: open artists keep their drawn
 * albums and tracks (`reuse`, off after a language change) or are drawn from
 * the cache.
 */
function renderArtists(reuse = true) {
  const drawn = new Map(
    reuse ? [...document.querySelectorAll("#artist-list > li.albums-wrap")].map((w) => [w.dataset.artist, w]) : [],
  );
  const all = state.artists || [];
  const { items, total, truncated } = limitMatches(all, $("artist-filter").value, ARTIST_LIMIT);
  const frag = document.createDocumentFragment();
  for (const name of items) {
    const open = state.expanded.has(name);
    const li = el("li", { className: open ? "artist open" : "artist" }, el("span", { className: "name", textContent: name || t("artists.unknown"), title: name }));
    li.dataset.artist = name;
    frag.append(li);
    const albums = open && state.albumCache.get(name);
    if (albums) frag.append(drawn.get(name) || albumsWrap(name, albums));
  }
  $("artist-list").replaceChildren(frag);
  const note = $("artist-note");
  if (!all.length) note.textContent = t("artists.none");
  else if (!total) note.textContent = t("artists.no_match");
  else if (truncated)
    note.textContent = t("artists.truncated", { shown: items.length, total });
  note.hidden = Boolean(total) && !truncated;
  // Open artists whose albums are not loaded yet.
  for (const li of document.querySelectorAll("#artist-list > li.artist.open")) {
    if (!state.albumCache.has(li.dataset.artist)) refreshAlbums(li);
  }
}

/** An artist's albums; `tracks` (album name -> drawn tracks) are reused. */
function albumsWrap(artist, albums, tracks = new Map()) {
  const ul = el("ul", { className: "albums" });
  for (const album of albums) {
    const a = el(
      "li",
      { className: "album" },
      el("span", { className: "name", textContent: albumLabel(album, t), title: t("albums.play") }),
      el("button", { type: "button", className: "tracks", textContent: "▾", title: t("albums.tracks") }),
      el("button", { type: "button", className: "add", textContent: "+", title: t("albums.more") }),
    );
    a.dataset.artist = artist;
    a.dataset.album = album.name;
    ul.append(a);
    if (state.openAlbums.has(albumKey(artist, album.name))) {
      a.classList.add("open");
      ul.append(tracks.get(album.name) || tracksWrap(a));
    }
  }
  if (!albums.length) ul.append(el("li", { className: "note", textContent: t("albums.none") }));
  const wrap = el("li", { className: "albums-wrap" }, ul);
  wrap.dataset.artist = artist;
  return wrap;
}

/** Reads an open artist's albums; redraws them only if they changed, then
 * refreshes the tracks of its open albums. */
async function refreshAlbums(li) {
  const artist = li.dataset.artist;
  const albums = await run("list_albums", { artist });
  if (!albums || !li.isConnected || !state.expanded.has(artist)) return;
  const changed = rowKey(albums) !== rowKey(state.albumCache.get(artist));
  state.albumCache.set(artist, albums);
  const next = li.nextElementSibling;
  const wrap = next && next.classList.contains("albums-wrap") ? next : null;
  if (changed || !wrap) {
    // The tracks already drawn under open albums are moved over, not redrawn.
    const drawn = new Map();
    for (const tw of wrap ? wrap.querySelectorAll("li.tracks-wrap") : []) {
      const album = tw.previousElementSibling;
      if (album && album.dataset.album !== undefined) drawn.set(album.dataset.album, tw);
    }
    keepAnchor(() => {
      const fresh = albumsWrap(artist, albums, drawn);
      if (wrap) wrap.replaceWith(fresh);
      else li.after(fresh);
    });
  }
  for (const a of li.nextElementSibling.querySelectorAll("li.album.open")) {
    const tracks = a.nextElementSibling && a.nextElementSibling.querySelector("ul.album-tracks");
    if (tracks) loadTracks(artist, a.dataset.album, tracks);
  }
}

function toggleAlbums(li) {
  const artist = li.dataset.artist;
  if (state.expanded.has(artist)) {
    state.expanded.delete(artist);
    const next = li.nextElementSibling;
    if (next && next.classList.contains("albums-wrap")) next.remove();
    li.classList.remove("open");
    return;
  }
  state.expanded.add(artist);
  li.classList.add("open");
  const cached = state.albumCache.get(artist);
  if (cached) li.after(albumsWrap(artist, cached));
  // Read again on every open (the library may have changed).
  refreshAlbums(li);
}

/** The tracks under an open album: from the cache at once, else when they arrive. */
function tracksWrap(albumLi) {
  const { artist, album } = albumLi.dataset;
  const ul = el("ul", { className: "album-tracks" });
  const cached = state.trackCache.get(albumKey(artist, album));
  if (cached) patchTracks(ul, cached, artist);
  else loadTracks(artist, album, ul);
  return el("li", { className: "tracks-wrap" }, ul);
}

async function loadTracks(artist, album, ul) {
  const tracks = await run("list_album_tracks", { artist, album });
  if (!tracks || !ul.isConnected) return;
  state.trackCache.set(albumKey(artist, album), tracks);
  keepAnchor(() => patchTracks(ul, tracks, artist));
}

/** Makes `ul` show `tracks`, replacing only the rows that changed. */
function patchTracks(ul, tracks, artist) {
  const keys = tracks.map(rowKey);
  const plan = patchPlan([...ul.children].map((li) => li.dataset.key || ""), keys);
  for (const i of plan.replace) ul.children[i].replaceWith(trackRow(tracks[i], artist));
  for (let n = 0; n < plan.remove; n += 1) ul.lastElementChild.remove();
  for (let i = keys.length - plan.add; i < keys.length; i += 1) ul.append(trackRow(tracks[i], artist));
}

function trackRow(tr, artist) {
  const label = [tr.track_no ? `${tr.track_no}.` : "", trackLabel({ ...tr, artist: tr.artist === artist ? "" : tr.artist })]
    .filter(Boolean)
    .join(" ");
  const li = el(
    "li",
    { className: "track" },
    el("span", { className: "name", textContent: label, title: trackLabel(tr) }),
    el("span", { className: "dur", textContent: tr.duration_ms ? formatTime(tr.duration_ms) : "" }),
    el("button", { type: "button", className: "more", textContent: "⋯", title: t("albums.more") }),
  );
  li.dataset.key = rowKey(tr);
  trackOf.set(li, tr);
  return li;
}

/** Track rows (Artists) -> their track. */
const trackOf = new WeakMap();

function toggleTracks(albumLi) {
  const { artist, album } = albumLi.dataset;
  const key = albumKey(artist, album);
  const next = albumLi.nextElementSibling;
  if (state.openAlbums.has(key)) {
    state.openAlbums.delete(key);
    albumLi.classList.remove("open");
    if (next && next.classList.contains("tracks-wrap")) next.remove();
    return;
  }
  state.openAlbums.add(key);
  albumLi.classList.add("open");
  const wrap = tracksWrap(albumLi);
  albumLi.after(wrap);
  // Shown from the cache: read again in case the album changed.
  if (state.trackCache.has(key)) loadTracks(artist, album, wrap.firstElementChild);
}

function albumMenu(artist, album) {
  const add = (at) => async () => {
    const n = await run("playlist_add_album", { artist, album, at });
    if (n !== undefined) toast(t("playlist.added", { n }));
  };
  return [
    { label: t("menu.play_album"), run: () => run("play_album", { artist, album }) },
    "-",
    { label: t("menu.play_next"), run: add("next") },
    { label: t("menu.add_to_end"), run: add("end") },
  ];
}

function onArtistClick(e) {
  const li = e.target.closest("li");
  if (!li) return;
  const button = e.target.closest("button");
  if (li.classList.contains("artist")) {
    toggleAlbums(li);
  } else if (li.classList.contains("album")) {
    const { artist, album } = li.dataset;
    if (button && button.classList.contains("tracks")) toggleTracks(li);
    else if (button && button.classList.contains("add")) {
      e.stopPropagation();
      const r = button.getBoundingClientRect();
      openCtx(albumMenu(artist, album), r.left, r.bottom);
    } else run("play_album", { artist, album });
  } else if (li.classList.contains("track") && trackOf.has(li)) {
    e.stopPropagation();
    openCtx(trackMenu(trackOf.get(li)), e.clientX, e.clientY);
  }
}

function onArtistMenu(e) {
  const li = e.target.closest("li");
  if (!li) return;
  if (li.classList.contains("album")) {
    e.preventDefault();
    openCtx(albumMenu(li.dataset.artist, li.dataset.album), e.clientX, e.clientY);
  } else if (li.classList.contains("track") && trackOf.has(li)) {
    e.preventDefault();
    openCtx(trackMenu(trackOf.get(li)), e.clientX, e.clientY);
  }
}

// ---------- unreadable files ----------

function renderUnreadableTab() {
  $("tab-btn-unreadable").textContent = unreadableTabLabel(state.unreadableCount, t);
}

async function loadUnreadableCount() {
  const n = await run("unreadable_count");
  if (n === undefined) return;
  state.unreadableCount = n;
  renderUnreadableTab();
}

async function loadUnreadable() {
  const rows = await run("list_unreadable");
  if (!rows) return;
  state.unreadable = rows;
  state.unreadableCount = rows.length;
  renderUnreadableTab();
  renderUnreadable();
}

function renderUnreadable() {
  const rows = state.unreadable || [];
  const lang = document.documentElement.lang || "en";
  const frag = document.createDocumentFragment();
  for (const row of rows) {
    const reason = reasonText(row.code, t);
    const meta = el("div", { className: "meta" });
    const parts = [reason, t("unreadable.fails", { n: row.fail_count }), formatWhen(row.at, lang)].filter(Boolean);
    meta.append(parts.join(" · "));
    if (row.excluded) meta.append(" · ", el("span", { className: "excluded", textContent: t("unreadable.excluded") }));
    if (row.message) meta.title = row.message;
    const li = el(
      "li",
      {},
      el("div", { className: "info" }, el("span", { className: "path", textContent: row.path, title: row.path }), meta),
      el(
        "div",
        { className: "actions" },
        el("button", { className: "copy", textContent: t("unreadable.copy") }),
        el("button", { className: "retry", textContent: t("unreadable.retry") }),
      ),
    );
    li.dataset.id = row.id;
    frag.append(li);
  }
  $("unreadable-list").replaceChildren(frag);
  $("unreadable-empty").hidden = rows.length > 0;
  $("btn-copy-all").disabled = !rows.length;
  $("btn-retry-all").disabled = !rows.length;
}

/** After `unreadable-changed`: the count always, the list when it is on screen. */
const refreshUnreadable = debounce(() => {
  if (state.tab === "unreadable") loadUnreadable();
  else loadUnreadableCount();
}, 300);

async function retryUnreadable(id) {
  try {
    await invoke("retry_unreadable", { id });
  } catch (e) {
    toast(String(e));
  }
  loadUnreadable();
}

// ---------- danger zone ----------

/**
 * Asks for confirmation. `check` (optional) adds a checkbox: `{ label, note(checked),
 * checked }`. `danger: false` makes the button a plain one. Resolves to
 * `{ checked }` when confirmed, null when cancelled.
 */
function confirmDialog({ title, text, okLabel, check, danger = true }) {
  return new Promise((resolve) => {
    const overlay = $("confirm");
    const box = $("confirm-check");
    const note = $("confirm-note");
    $("confirm-title").textContent = title;
    $("confirm-text").textContent = text;
    $("confirm-ok").textContent = okLabel;
    $("confirm-ok").className = danger ? "danger-solid" : "primary";
    $("confirm-check-row").hidden = !check;
    box.checked = Boolean(check && check.checked);
    const renderNote = () => {
      const value = check ? check.note(box.checked) : "";
      note.hidden = !value;
      note.textContent = value;
    };
    if (check) $("confirm-check-label").textContent = check.label;
    renderNote();
    overlay.hidden = false;
    $("confirm-cancel").focus();
    const finish = (value) => {
      overlay.hidden = true;
      $("confirm-form").onsubmit = null;
      $("confirm-cancel").onclick = null;
      box.onchange = null;
      overlay.onkeydown = null;
      resolve(value);
    };
    box.onchange = renderNote;
    $("confirm-form").onsubmit = (e) => {
      e.preventDefault();
      finish({ checked: box.checked });
    };
    $("confirm-cancel").onclick = () => finish(null);
    overlay.onkeydown = (e) => {
      if (e.key === "Escape") finish(null);
    };
  });
}

async function onReset() {
  const answer = await confirmDialog({
    title: t("settings.reset_title"),
    text: t("settings.reset_text"),
    okLabel: t("settings.reset_button"),
    check: {
      label: t("settings.reset_forget"),
      note: (forget) => t(forget ? "settings.reset_forget_text" : "settings.reset_keep_servers"),
    },
  });
  if (!answer) return;
  let settings;
  try {
    settings = await invoke("reset_settings", { forget_servers: answer.checked });
  } catch (e) {
    toast(String(e));
    // Part of it may have happened (passwords deleted): show what is saved now.
    settings = await run("get_settings");
    if (!settings) return;
  }
  state.settings = settings;
  state.configured = !missingUsername(settings.primary);
  renderSettings(settings);
  loadDevices();
  if (!state.configured) renderStatus(null);
  renderGenres();
  toast(t("settings.reset_done"));
}

async function onClear() {
  const answer = await confirmDialog({
    title: t("settings.clear_title"),
    text: t("settings.clear_text"),
    okLabel: t("settings.clear_button"),
  });
  if (!answer) return;
  try {
    await invoke("clear_library");
  } catch (e) {
    toast(String(e));
    return;
  }
  state.genres = [];
  state.selected.clear();
  state.artists = null;
  state.artistsStale = true;
  state.albumCache.clear();
  state.trackCache.clear();
  state.unreadable = null;
  state.unreadableCount = 0;
  if (state.settings) state.settings.last_selection = [];
  renderUnreadableTab();
  renderArtists();
  loadBanned();
  showTab("genres");
  renderGenres();
  toast(t("settings.clear_done"));
}

// ---------- settings ----------

const serverFields = ["host", "share", "root", "username", "domain"];

function fieldset(which) {
  return document.querySelector(`fieldset[data-server="${which}"]`);
}

/** The saved config of a server (null if not saved). */
function savedServer(which) {
  const s = state.settings;
  return s ? (which === "primary" ? s.primary : s.secondary) : null;
}

function updatePasswordHint(which) {
  const pw = fieldset(which).elements.password;
  pw.placeholder = passwordRequired(savedServer(which), readServer(which))
    ? t("settings.password_required")
    : t("settings.password_keep");
}

function fillServer(which, cfg) {
  const fs = fieldset(which);
  for (const f of serverFields) fs.elements[f].value = cfg[f] || "";
  fs.elements.password.value = "";
  fs.querySelector(".test-result").textContent = "";
  updatePasswordHint(which);
}

function readServer(which) {
  const fs = fieldset(which);
  const values = {};
  for (const f of serverFields) values[f] = fs.elements[f].value;
  return serverFromForm(values);
}

function renderSettings(s) {
  fillServer("primary", s.primary);
  fillServer("secondary", s.secondary || SUGGESTED_SECONDARY);
  $("use-secondary").checked = Boolean(s.secondary);
  fieldset("secondary").hidden = !s.secondary;
  $("notify").checked = s.notify;
  $("autostart").checked = s.autostart;
  $("crossfade-on").checked = s.crossfade_enabled;
  $("crossfade-secs").value = s.crossfade_secs;
  $("crossfade-secs").disabled = !s.crossfade_enabled;
  $("crossfade-label").textContent = t("settings.seconds", { n: s.crossfade_secs });
  $("normalize").checked = s.normalize;
  $("resume-autoplay").checked = s.resume_autoplay;
  renderLookupServices();
  $("volume").value = Math.round(s.volume * 100);
}

function renderLookupServices() {
  const sel = $("lookup-service");
  sel.replaceChildren(...state.lookupServices.map((s) => el("option", { value: s.id, textContent: s.name })));
  if (state.settings) sel.value = state.settings.lookup_service || "lastfm";
}

async function onTest(button) {
  const which = button.dataset.which;
  const fs = fieldset(which);
  const out = fs.querySelector(".test-result");
  const server = readServer(which);
  out.className = "test-result";
  if (!server.host || missingUsername(server)) {
    out.textContent = t("settings.test_missing");
    out.classList.add("bad");
    return;
  }
  out.textContent = t("settings.testing");
  button.disabled = true;
  try {
    const status = await invoke("test_connection", {
      which,
      server,
      password: fs.elements.password.value || null,
    });
    out.textContent = status === "ok" ? t("settings.test_ok") : connMessage(status, t);
    out.classList.add(status === "ok" ? "ok" : "bad");
  } catch (e) {
    out.textContent = String(e);
    out.classList.add("bad");
  } finally {
    button.disabled = false;
  }
}

async function onSave(e) {
  e.preventDefault();
  const result = $("save-result");
  result.className = "result bad";
  const primary = readServer("primary");
  const secondary = $("use-secondary").checked ? readServer("secondary") : null;
  for (const [which, srv] of [["primary", primary], ["secondary", secondary]]) {
    if (!srv) continue;
    const els = fieldset(which).elements;
    let missing = null;
    if (!srv.host) {
      missing = "host";
      result.textContent = t(`settings.missing_host_${which}`);
    } else if (missingUsername(srv)) {
      missing = "username";
      result.textContent = t(`settings.missing_user_${which}`);
    } else if (!els.password.value && passwordRequired(savedServer(which), srv)) {
      missing = "password";
      result.textContent = t(`settings.missing_password_${which}`);
    }
    if (missing) {
      els[missing].focus();
      return;
    }
  }
  const submit = e.submitter || document.querySelector("#servers-form button[type=submit]");
  submit.disabled = true;
  result.textContent = "";
  try {
    // Start from the live settings: volume and playback options change outside this form.
    const current = await invoke("get_settings");
    const settings = {
      ...current,
      primary,
      secondary,
      notify: $("notify").checked,
      autostart: $("autostart").checked,
    };
    // The monitor reports the new status right after the save, possibly
    // before the reply: show "checking" first, never after.
    renderStatus(null, true);
    await invoke("save_settings", {
      settings,
      primary_password: fieldset("primary").elements.password.value || null,
      secondary_password: secondary ? fieldset("secondary").elements.password.value || null : null,
    });
    state.settings = settings;
    state.configured = true;
    fillServer("primary", primary);
    if (secondary) fillServer("secondary", secondary);
    result.className = "result";
    result.textContent = t("settings.saved");
    // Empty catalog: fill it straight away. A running scan is cancelled by the
    // backend when the servers changed; start again once it has stopped.
    if (!state.genres.length) {
      if (state.scanning) state.rescanIfCancelled = true;
      else startScan();
    }
  } catch (err) {
    result.textContent = String(err);
    renderStatus((await run("get_connection_status")) || null);
  } finally {
    submit.disabled = false;
  }
}

async function loadDevices() {
  const names = await run("list_devices");
  if (!names) return;
  const saved = state.settings && state.settings.device;
  const sel = $("device");
  const options = [el("option", { value: "", textContent: t("settings.device_default") })];
  for (const n of names) options.push(el("option", { value: n, textContent: n }));
  if (saved && !names.includes(saved))
    options.push(el("option", { value: saved, textContent: t("settings.device_missing", { name: saved }) }));
  sel.replaceChildren(...options);
  sel.value = saved || "";
}

function sendCrossfade() {
  const enabled = $("crossfade-on").checked;
  const secs = Number($("crossfade-secs").value);
  $("crossfade-secs").disabled = !enabled;
  if (state.settings) Object.assign(state.settings, { crossfade_enabled: enabled, crossfade_secs: secs });
  run("set_crossfade", { enabled, secs });
}

async function loadBanned() {
  const rows = await run("list_banned");
  if (!rows) return;
  const frag = document.createDocumentFragment();
  for (const row of rows) {
    const label = trackLabel(row);
    const li = el(
      "li",
      {},
      el("span", { className: "name", textContent: label === "—" ? row.rel_path : label, title: row.rel_path }),
      el("button", { textContent: t("settings.restore") }),
    );
    li.dataset.id = row.id;
    frag.append(li);
  }
  $("banned-list").replaceChildren(frag);
  $("banned-empty").hidden = rows.length > 0;
}

// ---------- language ----------

/** Loads the texts of `lang` and applies them to the static markup. */
async function loadLanguage(lang) {
  const dict = (await run("get_locale", { lang })) || {};
  t = makeT(lang, dict, dict);
  document.documentElement.lang = lang;
  for (const node of document.querySelectorAll("[data-i18n]")) node.textContent = t(node.dataset.i18n);
  for (const attr of ["title", "placeholder", "aria-label"]) {
    for (const node of document.querySelectorAll(`[data-i18n-${attr}]`)) {
      node.setAttribute(attr, t(node.getAttribute(`data-i18n-${attr}`)));
    }
  }
}

async function loadLanguages(current) {
  const langs = (await run("list_languages")) || [];
  $("language").replaceChildren(...langs.map((l) => el("option", { value: l.code, textContent: l.name })));
  $("language").value = current;
}

/** Re-words everything already on screen after a language change. */
function rerender() {
  renderNowPlaying(state.nowPlaying);
  renderStatus(state.status, state.statusChecking);
  renderGenres();
  if (state.artists) keepAnchor(() => renderArtists(false));
  renderUnreadableTab();
  if (state.unreadable) renderUnreadable();
  const scan = state.scanText;
  if (scan) {
    const text = scan.done ? scanDoneText(scan.done, t) : scanProgressText(scan.progress, t);
    $("genre-scan").textContent = text;
    $("scan-status").textContent = text;
  }
  $("crossfade-label").textContent = t("settings.seconds", { n: Number($("crossfade-secs").value) });
  for (const which of ["primary", "secondary"]) updatePasswordHint(which);
  for (const out of document.querySelectorAll(".test-result, #save-result")) out.textContent = "";
  if (pl.facets) renderFacets();
  if (pl.meta) {
    renderPlaylistHead();
    renderPlaylistRows();
  }
  if (state.tab === "settings") {
    loadDevices();
    loadBanned();
  }
}

async function onLanguage(e) {
  const lang = e.target.value;
  try {
    await invoke("set_language", { lang });
  } catch (err) {
    toast(String(err));
    e.target.value = (state.settings && state.settings.language) || "en";
    return;
  }
  if (state.settings) state.settings.language = lang;
  await loadLanguage(lang);
  rerender();
}

// ---------- wiring ----------

function wire() {
  for (const b of document.querySelectorAll(".tab")) b.addEventListener("click", () => showTab(b.dataset.tab));

  $("btn-prev").addEventListener("click", () => run("previous"));
  $("btn-pause").addEventListener("click", () => run("toggle_pause"));
  $("np-label").addEventListener("click", () => {
    if (state.nowPlaying) run("lookup_current");
  });
  document.querySelector(".now").addEventListener("contextmenu", (e) => {
    if (e.target.closest("input")) return;
    e.preventDefault();
    nowPlayingMenu(e.clientX, e.clientY);
  });
  $("btn-next").addEventListener("click", () => run("next"));
  $("btn-ban").addEventListener("click", onBan);
  $("volume").addEventListener("input", (e) => sendVolume(Number(e.target.value) / 100));

  // Genres
  $("genre-filter").addEventListener("input", debounce(renderGenres, 80));
  $("btn-play-selected").addEventListener("click", () => playGenres([...state.selected]));
  $("btn-play-all").addEventListener("click", () => playGenres([]));
  const list = $("genre-list");
  list.addEventListener("click", (e) => {
    const li = e.target.closest("li");
    if (!li) return;
    if (e.target.classList.contains("name")) playGenres([li.dataset.key]);
    else if (e.target.classList.contains("more")) {
      const r = e.target.getBoundingClientRect();
      e.stopPropagation();
      openMenu(li.dataset.key, r.left, r.bottom);
    }
  });
  list.addEventListener("change", (e) => {
    const li = e.target.closest("li");
    if (!li) return;
    if (e.target.checked) state.selected.add(li.dataset.key);
    else state.selected.delete(li.dataset.key);
    renderSelection();
  });
  list.addEventListener("contextmenu", (e) => {
    const li = e.target.closest("li");
    if (!li) return;
    e.preventDefault();
    if (li.dataset.key !== UNKNOWN_KEY) openMenu(li.dataset.key, e.clientX, e.clientY);
  });
  $("menu").addEventListener("click", (e) => {
    const action = e.target.dataset.action;
    if (action) onMenuAction(action);
  });
  document.addEventListener("click", (e) => {
    if (!$("menu").contains(e.target)) $("menu").hidden = true;
    if (!$("ctx").contains(e.target)) closeCtx();
    const genres = $("pl-genres");
    if (genres.open && !genres.contains(e.target)) genres.open = false;
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      $("menu").hidden = true;
      closeCtx();
      $("pl-genres").open = false;
    }
  });
  document.querySelector("main").addEventListener("scroll", () => {
    $("menu").hidden = true;
    closeCtx();
    onMainScroll();
  });
  window.addEventListener("resize", onMainScroll);
  window.addEventListener("blur", closeCtx);

  // Playlist
  $("pl-filter").addEventListener("input", filterTyped);
  $("pl-genre-list").addEventListener("change", (e) => {
    const key = e.target.dataset.key;
    if (key === undefined) return;
    pl.genres = e.target.checked ? [...pl.genres, key] : pl.genres.filter((k) => k !== key);
    applyFilter();
  });
  for (const id of ["pl-year-from", "pl-year-to", "pl-unknown-year"]) $(id).addEventListener("change", applyFilter);
  $("pl-current").addEventListener("click", () => {
    if (!scrollToCurrent()) toast(t("err.nothing_playing"));
  });
  $("pl-list").addEventListener("click", onPlaylistClick);
  $("pl-list").addEventListener("contextmenu", onPlaylistMenu);
  $("pl-play-filtered").addEventListener("click", onPlayFiltered);
  $("pl-new").addEventListener("click", onNewPlaylist);
  $("pl-export").addEventListener("click", onExport);

  // Artists
  $("artist-filter").addEventListener("input", debounce(renderArtists, 120));
  $("artist-list").addEventListener("click", onArtistClick);
  $("artist-list").addEventListener("contextmenu", onArtistMenu);

  // Settings
  $("language").addEventListener("change", onLanguage);
  $("servers-form").addEventListener("submit", onSave);
  for (const b of document.querySelectorAll(".btn-test")) b.addEventListener("click", () => onTest(b));
  $("use-secondary").addEventListener("change", (e) => (fieldset("secondary").hidden = !e.target.checked));
  for (const which of ["primary", "secondary"])
    fieldset(which).addEventListener("input", () => updatePasswordHint(which));
  $("device").addEventListener("change", (e) => {
    const name = e.target.value || null;
    if (state.settings) state.settings.device = name;
    run("set_device", { name });
  });
  $("crossfade-on").addEventListener("change", sendCrossfade);
  $("crossfade-secs").addEventListener("input", (e) => ($("crossfade-label").textContent = t("settings.seconds", { n: Number(e.target.value) })));
  $("crossfade-secs").addEventListener("change", sendCrossfade);
  $("normalize").addEventListener("change", (e) => {
    if (state.settings) state.settings.normalize = e.target.checked;
    run("set_normalize", { b: e.target.checked });
  });
  $("resume-autoplay").addEventListener("change", (e) => {
    if (state.settings) state.settings.resume_autoplay = e.target.checked;
    run("set_resume_autoplay", { on: e.target.checked });
  });
  $("lookup-service").addEventListener("change", (e) => {
    if (state.settings) state.settings.lookup_service = e.target.value;
    run("set_lookup_service", { service: e.target.value });
    renderNowPlaying(state.nowPlaying);
  });
  $("btn-scan").addEventListener("click", startScan);
  $("btn-empty-scan").addEventListener("click", startScan);

  // Unreadable files
  $("unreadable-list").addEventListener("click", (e) => {
    const li = e.target.closest("li");
    if (!li || e.target.tagName !== "BUTTON") return;
    const row = (state.unreadable || []).find((r) => r.id === Number(li.dataset.id));
    if (!row) return;
    if (e.target.classList.contains("copy")) copyAndTell(row.url);
    else if (e.target.classList.contains("retry")) retryUnreadable(row.id);
  });
  $("unreadable-list").addEventListener("contextmenu", (e) => {
    const li = e.target.closest("li");
    const row = li && (state.unreadable || []).find((r) => r.id === Number(li.dataset.id));
    if (!row) return;
    e.preventDefault();
    openCtx(trackMenu(row, { extra: [{ label: t("menu.copy_path"), run: () => copyAndTell(row.url) }] }), e.clientX, e.clientY);
  });
  $("btn-copy-all").addEventListener("click", () => copyAndTell(copyAllText(state.unreadable || [], t)));
  $("btn-retry-all").addEventListener("click", () => retryUnreadable(null));

  // Danger zone
  $("btn-reset").addEventListener("click", onReset);
  $("btn-clear").addEventListener("click", onClear);
  $("btn-scan-cancel").addEventListener("click", () => run("cancel_scan"));
  $("banned-list").addEventListener("click", async (e) => {
    if (e.target.tagName !== "BUTTON") return;
    const li = e.target.closest("li");
    try {
      await invoke("unban", { id: Number(li.dataset.id) });
      li.remove();
      $("banned-empty").hidden = $("banned-list").children.length > 0;
    } catch (err) {
      toast(String(err));
    }
  });
}

async function init() {
  wire();
  // Texts first, so nothing is shown untranslated.
  const settings = await run("get_settings");
  const lang = (settings && settings.language) || "en";
  const [, , services] = await Promise.all([loadLanguage(lang), loadLanguages(lang), run("lookup_services")]);
  state.lookupServices = services || [];
  await Promise.all([
    listen("now-playing", (e) => renderNowPlaying(e.payload)),
    listen("connection-status", (e) => renderStatus(e.payload)),
    listen("scan-progress", (e) => renderScanProgress(e.payload)),
    listen("scan-done", (e) => renderScanDone(e.payload)),
    listen("genres-changed", () => {
      refreshGenres();
      // Genre names may have changed (a merge): the playlist's facets are read again.
      pl.facetsFor = "";
      state.artistsStale = true;
      if (state.tab === "artists") reloadArtists();
    }),
    listen("track-skipped", (e) => console.info("skipped:", e.payload)),
    listen("unreadable-changed", () => refreshUnreadable()),
    listen("position", (e) => onPosition(e.payload)),
    listen("playlist-changed", () => onPlaylistChanged()),
  ]);

  if (settings) {
    state.settings = settings;
    state.configured = !missingUsername(settings.primary);
    renderSettings(settings);
  }
  // A tab the user already picked while starting up stays.
  if (!state.tab) showTab(state.configured ? "genres" : "settings");

  const [status, scan, np, position] = await Promise.all([
    run("get_connection_status"),
    run("get_scan_status"),
    run("now_playing"),
    run("get_position"),
  ]);
  renderStatus(status || null);
  state.position = position || null;
  renderNowPlaying(np || null);
  if (scan && scan.scanning) renderScanProgress(scan.progress);
  renderUnreadableTab();
  loadUnreadableCount();
  await loadGenres();
}

init();
