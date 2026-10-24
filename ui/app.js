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
} from "./lib.mjs";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ARTIST_LIMIT = 300;
const BAN_CONFIRM_MS = 3000;
const SUGGESTED_SECONDARY = { host: "10.111.0.2", share: "F", root: "MP3", username: "", domain: null };

const $ = (id) => document.getElementById(id);

const state = {
  tab: null,
  settings: null,
  configured: false,
  nowPlaying: null,
  genres: [],
  selected: new Set(),
  artists: null,
  artistsStale: true,
  scanning: false,
  rescanIfCancelled: false,
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
  $("btn-pause").title = np && np.paused ? "Nastavi" : "Pauza";
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
    b.textContent = "Sigurno?";
    banTimer = setTimeout(resetBan, BAN_CONFIRM_MS);
    return;
  }
  const label = trackLabel(state.nowPlaying);
  resetBan();
  try {
    await invoke("ban_current");
    toast(`Zabranjeno: ${label}`);
    if (state.tab === "settings") loadBanned();
  } catch (e) {
    toast(String(e));
  }
}

const sendVolume = debounce((v) => run("set_volume", { v }), 60);

// ---------- connection status ----------

function renderStatus(payload, checking = false) {
  const dot = $("conn-dot");
  dot.className = "dot" + (payload ? ` ${payload.status}` : "");
  $("conn-text").textContent = payload
    ? payload.message || connMessage(payload.status)
    : state.configured || checking
      ? "Provjeravam vezu…"
      : "Server nije postavljen";
}

// ---------- scan ----------

let scanNoteTimer = null;
function renderScanProgress(progress) {
  const started = !state.scanning;
  state.scanning = true;
  if (started) renderGenres();
  const text = scanProgressText(progress);
  clearTimeout(scanNoteTimer);
  $("genre-scan").textContent = text;
  $("genre-scan").hidden = false;
  $("scan-status").textContent = text;
  $("btn-scan").disabled = true;
  $("btn-scan-cancel").hidden = false;
}

function renderScanDone(done) {
  state.scanning = false;
  const text = scanDoneText(done);
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
  $("genre-names").replaceChildren(...genres.map((g) => el("option", { value: g.display })));
}

const refreshGenres = debounce(loadGenres, 500);

function renderGenres() {
  const shown = filterGenres(state.genres, $("genre-filter").value);
  const frag = document.createDocumentFragment();
  for (const g of shown) {
    const li = el(
      "li",
      {},
      el("input", { type: "checkbox", checked: state.selected.has(g.key), title: "Odaberi" }),
      el("span", { className: "name", textContent: g.display, title: `Pusti: ${g.display}` }),
      el("span", { className: "count", textContent: formatCount(g.count) }),
      el("button", { className: "more", textContent: "⋯", title: "Spoji ili preimenuj" }),
    );
    li.dataset.key = g.key;
    frag.append(li);
  }
  $("genre-list").replaceChildren(frag);

  const empty = $("genre-empty");
  if (!state.genres.length) {
    empty.textContent = state.scanning
      ? "Žanrovi će se pojavljivati tijekom skeniranja."
      : state.configured
        ? "Katalog je prazan. Osvježi katalog u Postavkama."
        : "Katalog je prazan. Postavi server u Postavkama.";
  } else if (!shown.length) {
    empty.textContent = "Nijedan žanr ne odgovara filtru.";
  }
  empty.hidden = Boolean(shown.length);
  $("btn-play-all").disabled = !state.genres.length;
  renderSelection();
}

function renderSelection() {
  const n = state.selected.size;
  const b = $("btn-play-selected");
  b.disabled = n === 0;
  b.textContent = n ? `Pusti odabrano (${n})` : "Pusti odabrano";
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

/** Asks for a genre name; resolves to the trimmed text, or null if cancelled. */
function askName(title, initial) {
  return new Promise((resolve) => {
    const overlay = $("dialog");
    const form = $("dialog-form");
    const input = $("dialog-input");
    $("dialog-title").textContent = title;
    input.value = initial;
    overlay.hidden = false;
    input.focus();
    input.select();
    const finish = (value) => {
      overlay.hidden = true;
      form.onsubmit = null;
      $("dialog-cancel").onclick = null;
      overlay.onkeydown = null;
      resolve(value);
    };
    form.onsubmit = (e) => {
      e.preventDefault();
      finish(input.value.trim() || null);
    };
    $("dialog-cancel").onclick = () => finish(null);
    overlay.onkeydown = (e) => {
      if (e.key === "Escape") finish(null);
    };
  });
}

async function onMenuAction(action) {
  const menu = $("menu");
  menu.hidden = true;
  const g = genreOf(menu.dataset.key);
  if (!g) return;
  const into =
    action === "merge"
      ? await askName(`Spoji „${g.display}“ u žanr:`, "")
      : await askName(`Novi naziv za „${g.display}“:`, g.display);
  if (!into || into === g.display) return;
  try {
    await invoke("merge_genre", { from_key: g.key, into_display: into });
    state.selected.delete(g.key);
    toast(action === "merge" ? `„${g.display}“ je spojen u „${into}“.` : `Preimenovano u „${into}“.`);
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
    const li = el("li", { className: "artist" }, el("span", { className: "name", textContent: name || "(nepoznat izvođač)", title: name }));
    li.dataset.artist = name;
    frag.append(li);
  }
  $("artist-list").replaceChildren(frag);
  const note = $("artist-note");
  if (!all.length) note.textContent = "Nema izvođača u katalogu.";
  else if (!total) note.textContent = "Nijedan izvođač ne odgovara pretrazi.";
  else if (truncated)
    note.textContent = `Prikazano ${formatCount(items.length)} od ${formatCount(total)}. Suzi pretragu.`;
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
    const a = el("li", {}, el("span", { className: "name", textContent: album || "(bez albuma)", title: "Pusti album" }));
    a.dataset.album = album;
    ul.append(a);
  }
  if (!albums.length) ul.append(el("li", { className: "note", textContent: "Nema albuma." }));
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
    ? "obavezno"
    : "prazno = ostaje spremljena";
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
  $("crossfade-label").textContent = `${s.crossfade_secs} s`;
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
    out.textContent = "Upiši server i korisničko ime.";
    out.classList.add("bad");
    return;
  }
  out.textContent = "Testiram…";
  button.disabled = true;
  try {
    const status = await invoke("test_connection", {
      which,
      server,
      password: fs.elements.password.value || null,
    });
    out.textContent = status === "ok" ? "Veza radi." : connMessage(status);
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
  for (const [which, srv, name] of [["primary", primary, "glavnog"], ["secondary", secondary, "rezervnog"]]) {
    if (!srv) continue;
    const els = fieldset(which).elements;
    let missing = null;
    if (!srv.host) {
      missing = "host";
      result.textContent = `Upiši adresu ${name} servera.`;
    } else if (missingUsername(srv)) {
      missing = "username";
      result.textContent = `Upiši korisničko ime ${name} servera.`;
    } else if (!els.password.value && passwordRequired(savedServer(which), srv)) {
      missing = "password";
      result.textContent = `Upiši lozinku ${name} servera: server, share ili korisnik su novi, pa spremljene lozinke nema.`;
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
    result.textContent = "Spremljeno.";
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
  const options = [el("option", { value: "", textContent: "Zadani uređaj sustava" })];
  for (const n of names) options.push(el("option", { value: n, textContent: n }));
  if (saved && !names.includes(saved))
    options.push(el("option", { value: saved, textContent: `${saved} (nije spojen)` }));
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
      el("button", { textContent: "Vrati" }),
    );
    li.dataset.id = t.id;
    frag.append(li);
  }
  $("banned-list").replaceChildren(frag);
  $("banned-empty").hidden = rows.length > 0;
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
    openMenu(li.dataset.key, e.clientX, e.clientY);
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
  $("crossfade-secs").addEventListener("input", (e) => ($("crossfade-label").textContent = `${e.target.value} s`));
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
    listen("track-skipped", (e) => console.info("preskočeno:", e.payload)),
  ]);

  const settings = await run("get_settings");
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
