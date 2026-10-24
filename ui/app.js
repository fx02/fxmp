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
  if (name === "settings") {
    loadDevices();
    loadBanned();
  }
}

// ---------- now playing ----------

function renderNowPlaying(np) {
  const changed = (np && np.id) !== (state.nowPlaying && state.nowPlaying.id);
  state.nowPlaying = np;
  const label = $("np-label");
  label.textContent = trackLabel(np);
  label.title = np ? [trackLabel(np), np.album].filter(Boolean).join(" · ") : "";
  label.classList.toggle("paused", Boolean(np && np.paused));
  const chip = $("np-genre");
  chip.textContent = (np && np.genre_display) || "";
  chip.title = chip.textContent;
  chip.hidden = !chip.textContent;
  $("btn-pause").title = np && np.paused ? t("player.resume") : t("player.pause");
  $("btn-ban").disabled = !np;
  // A pending ban confirmation was for the previous track.
  if (changed) resetBan();
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

async function loadArtists() {
  if (state.artists && !state.artistsStale) return;
  state.artistsStale = false;
  const artists = await run("list_artists");
  if (!artists) return;
  state.artists = artists;
  renderArtists();
}

const reloadArtists = debounce(loadArtists, 1000);

function renderArtists() {
  const all = state.artists || [];
  const { items, total, truncated } = limitMatches(all, $("artist-filter").value, ARTIST_LIMIT);
  const frag = document.createDocumentFragment();
  for (const name of items) {
    const li = el("li", { className: "artist" }, el("span", { className: "name", textContent: name || t("artists.unknown"), title: name }));
    li.dataset.artist = name;
    frag.append(li);
  }
  $("artist-list").replaceChildren(frag);
  const note = $("artist-note");
  if (!all.length) note.textContent = t("artists.none");
  else if (!total) note.textContent = t("artists.no_match");
  else if (truncated)
    note.textContent = t("artists.truncated", { shown: items.length, total });
  note.hidden = Boolean(total) && !truncated;
}

async function toggleAlbums(li) {
  const next = li.nextElementSibling;
  if (next && next.classList.contains("albums-wrap")) {
    next.remove();
    li.classList.remove("open");
    return;
  }
  const artist = li.dataset.artist;
  const albums = await run("list_albums", { artist });
  if (!albums || !li.isConnected) return;
  const ul = el("ul", { className: "albums" });
  for (const album of albums) {
    const a = el("li", {}, el("span", { className: "name", textContent: album || t("albums.unknown"), title: t("albums.play") }));
    a.dataset.album = album;
    ul.append(a);
  }
  if (!albums.length) ul.append(el("li", { className: "note", textContent: t("albums.none") }));
  const wrap = el("li", { className: "albums-wrap" }, ul);
  wrap.dataset.artist = artist;
  li.after(wrap);
  li.classList.add("open");
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
  $("volume").value = Math.round(s.volume * 100);
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
  for (const t of rows) {
    const label = trackLabel(t);
    const li = el(
      "li",
      {},
      el("span", { className: "name", textContent: label === "—" ? t.rel_path : label, title: t.rel_path }),
      el("button", { textContent: t("settings.restore") }),
    );
    li.dataset.id = t.id;
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
  if (state.artists) renderArtists();
  const scan = state.scanText;
  if (scan) {
    const text = scan.done ? scanDoneText(scan.done, t) : scanProgressText(scan.progress, t);
    $("genre-scan").textContent = text;
    $("scan-status").textContent = text;
  }
  $("crossfade-label").textContent = t("settings.seconds", { n: Number($("crossfade-secs").value) });
  for (const which of ["primary", "secondary"]) updatePasswordHint(which);
  for (const out of document.querySelectorAll(".test-result, #save-result")) out.textContent = "";
  for (const b of document.querySelectorAll("#tab-artists .albums-wrap")) b.remove();
  for (const li of document.querySelectorAll("#tab-artists li.open")) li.classList.remove("open");
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

  $("btn-pause").addEventListener("click", () => run("toggle_pause"));
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
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") $("menu").hidden = true;
  });
  document.querySelector("main").addEventListener("scroll", () => ($("menu").hidden = true));

  // Artists
  $("artist-filter").addEventListener("input", debounce(renderArtists, 120));
  $("artist-list").addEventListener("click", (e) => {
    const li = e.target.closest("li");
    if (!li) return;
    if (li.classList.contains("artist")) toggleAlbums(li);
    else if (li.dataset.album !== undefined) {
      const artist = li.closest(".albums-wrap").dataset.artist;
      run("play_album", { artist, album: li.dataset.album });
    }
  });

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
  $("btn-scan").addEventListener("click", startScan);
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
  await Promise.all([loadLanguage(lang), loadLanguages(lang)]);
  await Promise.all([
    listen("now-playing", (e) => renderNowPlaying(e.payload)),
    listen("connection-status", (e) => renderStatus(e.payload)),
    listen("scan-progress", (e) => renderScanProgress(e.payload)),
    listen("scan-done", (e) => renderScanDone(e.payload)),
    listen("genres-changed", () => {
      refreshGenres();
      state.artistsStale = true;
      if (state.tab === "artists") reloadArtists();
    }),
    listen("track-skipped", (e) => console.info("skipped:", e.payload)),
  ]);

  if (settings) {
    state.settings = settings;
    state.configured = !missingUsername(settings.primary);
    renderSettings(settings);
  }
  showTab(state.configured ? "genres" : "settings");

  const [status, scan, np] = await Promise.all([
    run("get_connection_status"),
    run("get_scan_status"),
    run("now_playing"),
  ]);
  renderStatus(status || null);
  renderNowPlaying(np || null);
  if (scan && scan.scanning) renderScanProgress(scan.progress);
  await loadGenres();
}

init();
