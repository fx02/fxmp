//! Menu bar (tray) icon and its menu.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use smp_core::audio::output::list_output_devices;
use smp_core::audio::player::Command;
use smp_core::catalog::GenreCount;
use smp_core::genre::UNKNOWN_KEY;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Listener, Manager, Wry};

use crate::commands::{
    ban_current_now, play_genres_now, play_pause_now, previous_now, set_device_now, set_volume_now,
    spawn_action,
};
use crate::i18n::{self, tr, trf};
use crate::lookup;
use crate::state::{self, lock, AppState, NowPlaying};
use crate::tray_click::{self, ClickAction, ClickClassifier};

/// Genres change in bursts (every few seconds during a scan): the submenu is rebuilt
/// once they settle for this long.
const GENRE_MENU_DEBOUNCE: Duration = Duration::from_secs(1);
const TOOLTIP_IDLE: &str = "SMB Music Player";
const ID_TOGGLE: &str = "toggle";
const ID_PREVIOUS: &str = "previous";
const ID_NEXT: &str = "next";
const ID_BAN: &str = "ban";
const ID_LOOKUP: &str = "lookup";
const ID_COPY: &str = "copy";
const ID_SHOW: &str = "show";
const ID_QUIT: &str = "quit";
const VOLUME_ID_PREFIX: &str = "vol:";
const OUTPUT_DEFAULT_ID: &str = "out-default";
const OUTPUT_ID_PREFIX: &str = "out:";

/// Genres in the "Genre" submenu besides "All" and the last selection.
pub const TOP_GENRES: usize = 30;
const ALL_GENRES_ID: &str = "genre-all";
const GENRE_ID_PREFIX: &str = "genre:";
/// Longest track label in the menu, in characters.
const LABEL_MAX_CHARS: usize = 60;

/// One item of the "Genre" submenu.
#[derive(Debug, Clone, PartialEq)]
pub struct GenreEntry {
    pub id: String,
    pub label: String,
}

/// A check item of the "Volume" or "Output" submenu.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckEntry {
    pub id: String,
    pub label: String,
    pub checked: bool,
}

/// What a "Genre" submenu item plays.
#[derive(Debug, Clone, PartialEq)]
pub enum GenrePick {
    All,
    One(String),
}

/// The tray icon and the menu items that change.
struct Tray {
    icon: TrayIcon,
    now: MenuItem<Wry>,
    pause: MenuItem<Wry>,
    previous: MenuItem<Wry>,
    next: MenuItem<Wry>,
    ban: MenuItem<Wry>,
    lookup: MenuItem<Wry>,
    copy: MenuItem<Wry>,
    volume: Submenu<Wry>,
    output: Submenu<Wry>,
    genres: Submenu<Wry>,
    show: MenuItem<Wry>,
    quit: MenuItem<Wry>,
    rebuild_genres: Sender<()>,
    rebuild_audio: Sender<()>,
    clicks: Mutex<ClickClassifier>,
}

/// Creates the tray icon. Runs on the main thread, after `AppState::init`.
pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let lang = i18n::current();
    let item = |id: &str, key: &str, enabled: bool| {
        MenuItem::with_id(app, id, menu_text(&tr(&lang, key)), enabled, None::<&str>)
    };
    let now = MenuItem::with_id(
        app,
        "now",
        menu_text(&track_label(&lang, None)),
        false,
        None::<&str>,
    )?;
    let pause = MenuItem::with_id(
        app,
        ID_TOGGLE,
        pause_label(&lang, None),
        false,
        None::<&str>,
    )?;
    let previous = item(ID_PREVIOUS, "player.previous", false)?;
    let next = item(ID_NEXT, "player.next", true)?;
    let ban = item(ID_BAN, "player.ban", false)?;
    let lookup = item(ID_LOOKUP, "tray.lookup_current", false)?;
    let copy = item(ID_COPY, "tray.copy_current", false)?;
    // Filled in the background: neither the catalog nor the audio devices are
    // queried on the main thread.
    let volume = Submenu::with_items(app, menu_text(&tr(&lang, "tray.volume")), true, &[])?;
    let output = Submenu::with_items(app, menu_text(&tr(&lang, "tray.output")), true, &[])?;
    let genres = Submenu::with_items(app, menu_text(&tr(&lang, "tray.genre")), true, &[])?;
    let show = item(ID_SHOW, "tray.show", true)?;
    let quit = item(ID_QUIT, "tray.quit", true)?;
    let version = MenuItem::with_id(
        app,
        "version",
        menu_text(&crate::about::about(app).text),
        false,
        None::<&str>,
    )?;
    let menu = Menu::with_items(
        app,
        &[
            &now,
            &PredefinedMenuItem::separator(app)?,
            &pause,
            &previous,
            &next,
            &ban,
            &PredefinedMenuItem::separator(app)?,
            &lookup,
            &copy,
            &PredefinedMenuItem::separator(app)?,
            &genres,
            &volume,
            &output,
            &PredefinedMenuItem::separator(app)?,
            &show,
            &quit,
            &PredefinedMenuItem::separator(app)?,
            &version,
        ],
    )?;
    let window = tray_click::clamp_window(tray_click::system_interval());
    log::info!("tray: double-click window {} ms", window.as_millis());
    let icon = TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(include_bytes!(
            "../icons/tray-next@2x.png"
        ))?)
        .icon_as_template(true)
        .tooltip(TOOLTIP_IDLE)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(|tray, event| on_icon_event(tray.app_handle(), event))
        .build(app)?;

    let (tx, rx) = mpsc::channel();
    let (audio_tx, audio_rx) = mpsc::channel();
    app.manage(Tray {
        icon,
        now,
        pause,
        previous,
        next,
        ban,
        lookup,
        copy,
        volume,
        output,
        genres,
        show,
        quit,
        rebuild_genres: tx,
        rebuild_audio: audio_tx,
        clicks: Mutex::new(ClickClassifier::new(window)),
    });
    spawn_audio_menu_thread(app, audio_rx)?;
    refresh_audio(app);

    let handle = app.clone();
    std::thread::Builder::new()
        .name("tray-genres".into())
        .spawn(move || {
            let mut shown = Vec::new();
            while rx.recv().is_ok() {
                std::thread::sleep(GENRE_MENU_DEBOUNCE);
                while rx.try_recv().is_ok() {}
                if let Err(e) = rebuild_genre_menu(&handle, &mut shown) {
                    log::warn!("tray genre menu: {e}");
                }
            }
        })?;
    refresh_genres(app);
    let handle = app.clone();
    app.listen_any("genres-changed", move |_| refresh_genres(&handle));
    let handle = app.clone();
    state::on_now_playing(app, move |np| show_now_playing(&handle, np));
    Ok(())
}

/// Left clicks: one is "Next" (after the double-click window), two are
/// "Play/Pause". A right click opens the menu; the device list is read again for
/// the next time.
fn on_icon_event(app: &AppHandle, event: TrayIconEvent) {
    let now = Instant::now();
    let action = match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } => clicks(app, |c| c.click(now)),
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => clicks(app, |c| c.double_click(now)),
        TrayIconEvent::Click {
            button: MouseButton::Right,
            button_state: MouseButtonState::Down,
            ..
        } => {
            refresh_audio(app);
            None
        }
        _ => None,
    };
    match action {
        Some(ClickAction::Toggle) => {
            log::debug!("tray: double click, play/pause");
            spawn_action(app, "play/pause", |_, st| {
                play_pause_now(st, Command::TogglePause);
                Ok(())
            });
        }
        Some(ClickAction::Wait(seq)) => {
            let handle = app.clone();
            let window = clicks(app, |c| c.window()).unwrap_or(tray_click::MAX_WINDOW);
            let spawned = std::thread::Builder::new()
                .name("tray-click".into())
                .spawn(move || {
                    std::thread::sleep(window);
                    if clicks(&handle, |c| c.due(seq, Instant::now())) == Some(true) {
                        log::debug!("tray: single click, next track");
                        handle.state::<AppState>().player.send(Command::Next);
                    }
                });
            if let Err(e) = spawned {
                log::warn!("tray click: {e}");
            }
        }
        Some(ClickAction::Ignore) | None => {}
    }
}

fn clicks<T>(app: &AppHandle, f: impl FnOnce(&mut ClickClassifier) -> T) -> Option<T> {
    app.try_state::<Tray>().map(|t| f(&mut lock(&t.clicks)))
}

/// Schedules a refresh of the "Volume" and "Output" submenus: after a volume or
/// device change (from anywhere), a settings change, a language change, and when
/// the menu is opened. No-op before the tray exists.
pub fn refresh_audio(app: &AppHandle) {
    if let Some(tray) = app.try_state::<Tray>() {
        let _ = tray.rebuild_audio.send(());
    }
}

/// The devices are listed on this thread, never on the main or the audio thread.
fn spawn_audio_menu_thread(app: &AppHandle, rx: Receiver<()>) -> tauri::Result<()> {
    let handle = app.clone();
    std::thread::Builder::new()
        .name("tray-audio".into())
        .spawn(move || {
            let mut shown_volume = Vec::new();
            let mut shown_output = Vec::new();
            while rx.recv().is_ok() {
                while rx.try_recv().is_ok() {}
                let lang = i18n::current();
                let settings = handle.state::<AppState>().settings.get();
                let devices = list_output_devices();
                let volume = volume_entries(&lang, settings.volume);
                let output = output_entries(&lang, &devices, settings.device.as_deref());
                let tray = handle.state::<Tray>();
                for (menu, entries, shown) in [
                    (&tray.volume, volume, &mut shown_volume),
                    (&tray.output, output, &mut shown_output),
                ] {
                    if let Err(e) = show_check_entries(&handle, menu, entries, shown) {
                        log::warn!("tray audio menu: {e}");
                    }
                }
            }
        })?;
    Ok(())
}

/// Shows `entries` in `menu`: only the checks change when the items are the same
/// as `shown`, otherwise the items are replaced.
fn show_check_entries(
    app: &AppHandle,
    menu: &Submenu<Wry>,
    entries: Vec<CheckEntry>,
    shown: &mut Vec<CheckEntry>,
) -> Result<(), String> {
    let err = |e: tauri::Error| e.to_string();
    let same_items = shown.len() == entries.len()
        && shown
            .iter()
            .zip(&entries)
            .all(|(a, b)| a.id == b.id && a.label == b.label);
    if same_items {
        // A click on a check item toggles its mark by itself: every mark is set.
        for (item, e) in menu.items().map_err(err)?.iter().zip(&entries) {
            if let Some(check) = item.as_check_menuitem() {
                check.set_checked(e.checked).map_err(err)?;
            }
        }
    } else {
        // Until the rebuild completes the submenu matches nothing, so a failure is
        // retried on the next refresh.
        shown.clear();
        for item in menu.items().map_err(err)? {
            menu.remove(&item).map_err(err)?;
        }
        for e in &entries {
            let item = CheckMenuItem::with_id(
                app,
                &e.id,
                menu_text(&e.label),
                true,
                e.checked,
                None::<&str>,
            )
            .map_err(err)?;
            menu.append(&item).map_err(err)?;
        }
    }
    *shown = entries;
    Ok(())
}

/// Schedules a rebuild of the "Genre" submenu (no-op before the tray exists).
pub fn refresh_genres(app: &AppHandle) {
    if let Some(tray) = app.try_state::<Tray>() {
        let _ = tray.rebuild_genres.send(());
    }
}

/// Replaces the "Genre" items unless they would stay the same as `shown`.
fn rebuild_genre_menu(app: &AppHandle, shown: &mut Vec<GenreEntry>) -> Result<(), String> {
    let st = app.state::<AppState>();
    let genres = lock(&st.query).genres().map_err(|e| e.to_string())?;
    let entries = genre_entries(&i18n::current(), &genres, &st.settings.get().last_selection);
    if entries == *shown {
        return Ok(());
    }
    let menu = &app.state::<Tray>().genres;
    let err = |e: tauri::Error| e.to_string();
    // Until the rebuild completes the submenu matches nothing, so a failure is
    // retried on the next event.
    shown.clear();
    for item in menu.items().map_err(err)? {
        menu.remove(&item).map_err(err)?;
    }
    for e in &entries {
        let item =
            MenuItem::with_id(app, &e.id, menu_text(&e.label), true, None::<&str>).map_err(err)?;
        menu.append(&item).map_err(err)?;
    }
    log::info!("tray genre menu: {} items", entries.len());
    *shown = entries;
    Ok(())
}

/// Updates the menu and tooltip in one main-thread call, so the player-event thread
/// does not wait for the main thread.
fn show_now_playing(app: &AppHandle, np: Option<NowPlaying>) {
    let handle = app.clone();
    let result = app.run_on_main_thread(move || {
        let Some(tray) = handle.try_state::<Tray>() else {
            return;
        };
        let np = np.as_ref();
        let lang = i18n::current();
        let label = track_label(&lang, np);
        let _ = tray.now.set_text(menu_text(&label));
        let _ = tray.pause.set_text(pause_label(&lang, np));
        for item in [
            &tray.pause,
            &tray.previous,
            &tray.ban,
            &tray.lookup,
            &tray.copy,
        ] {
            let _ = item.set_enabled(np.is_some());
        }
        let tooltip = if np.is_some() {
            label.as_str()
        } else {
            TOOLTIP_IDLE
        };
        let _ = tray.icon.set_tooltip(Some(tooltip));
    });
    if let Err(e) = result {
        log::warn!("tray: {e}");
    }
}

/// Relabels the menu after a language change.
pub fn relabel(app: &AppHandle) {
    let handle = app.clone();
    let result = app.run_on_main_thread(move || {
        let Some(tray) = handle.try_state::<Tray>() else {
            return;
        };
        let lang = i18n::current();
        let text = |key: &str| menu_text(&tr(&lang, key));
        let _ = tray.previous.set_text(text("player.previous"));
        let _ = tray.next.set_text(text("player.next"));
        let _ = tray.ban.set_text(text("player.ban"));
        let _ = tray.lookup.set_text(text("tray.lookup_current"));
        let _ = tray.copy.set_text(text("tray.copy_current"));
        let _ = tray.volume.set_text(text("tray.volume"));
        let _ = tray.output.set_text(text("tray.output"));
        let _ = tray.genres.set_text(text("tray.genre"));
        let _ = tray.show.set_text(text("tray.show"));
        let _ = tray.quit.set_text(text("tray.quit"));
    });
    if let Err(e) = result {
        log::warn!("tray: {e}");
    }
    let np = lock(&app.state::<AppState>().now_playing).clone();
    show_now_playing(app, np);
    // The genre entries now differ ("All", "Unknown"), so the submenu is rebuilt;
    // so do the volume steps and "System default".
    refresh_genres(app);
    refresh_audio(app);
}

/// Runs on the main thread: anything that touches the catalog goes to a thread.
fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref();
    let st = app.state::<AppState>();
    match id {
        ID_TOGGLE => spawn_action(app, "play/pause", |_, st| {
            play_pause_now(st, Command::TogglePause);
            Ok(())
        }),
        ID_PREVIOUS => spawn_action(app, "previous", |_, st| previous_now(st)),
        ID_NEXT => st.player.send(Command::Next),
        ID_BAN => spawn_action(app, "ban", ban_current_now),
        ID_LOOKUP => spawn_action(app, "look up", lookup::open_current),
        ID_COPY => spawn_action(app, "copy", copy_current),
        ID_SHOW => show_main_window(app),
        ID_QUIT => app.exit(0),
        _ => {
            if let Some(v) = volume_pick(id) {
                spawn_action(app, "volume", move |app, st| {
                    set_volume_now(app, st, v);
                    Ok(())
                });
            } else if let Some(device) = output_pick(id) {
                spawn_action(app, "output", move |app, st| {
                    set_device_now(app, st, device);
                    Ok(())
                });
            } else if let Some(pick) = genre_pick(id) {
                let keys = match pick {
                    GenrePick::All => Vec::new(),
                    GenrePick::One(key) => vec![key],
                };
                spawn_action(app, "play genre", move |app, st| {
                    play_genres_now(app, st, keys)
                });
            }
        }
    }
}

/// "Copy current song": "Artist – Title (Year)" on the clipboard.
fn copy_current(app: &AppHandle, st: &AppState) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let text = lookup::current_text(st).ok_or_else(|| i18n::t("err.nothing_playing"))?;
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = handle.clipboard().write_text(text) {
            log::warn!("clipboard: {e}");
        }
    })
    .map_err(|e| e.to_string())
}

/// Shows, restores and focuses the main window.
pub fn show_main_window(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let _ = w.show();
    let _ = w.unminimize();
    let _ = w.set_focus();
}

/// "All", then the [`TOP_GENRES`] largest genres by track count, then the genres of
/// `last_selection` that are not among them. "All" and Unknown are in `lang`.
pub fn genre_entries(
    lang: &str,
    genres: &[GenreCount],
    last_selection: &[String],
) -> Vec<GenreEntry> {
    let mut by_count: Vec<&GenreCount> = genres.iter().collect();
    by_count.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
            .then_with(|| a.key.cmp(&b.key))
    });
    let entry = |g: &GenreCount| GenreEntry {
        id: format!("{GENRE_ID_PREFIX}{}", g.key),
        label: if g.key == UNKNOWN_KEY {
            tr(lang, "genres.unknown")
        } else {
            g.display.clone()
        },
    };
    let mut entries = vec![GenreEntry {
        id: ALL_GENRES_ID.into(),
        label: tr(lang, "genres.all"),
    }];
    entries.extend(by_count.iter().take(TOP_GENRES).map(|g| entry(g)));
    entries.extend(
        by_count
            .iter()
            .skip(TOP_GENRES)
            .filter(|g| last_selection.contains(&g.key))
            .map(|g| entry(g)),
    );
    entries
}

/// The volume rounded to the nearest 10 %, 0..=100.
pub fn volume_step(volume: f32) -> u8 {
    if !volume.is_finite() {
        return 0;
    }
    ((volume.clamp(0.0, 1.0) * 10.0).round() * 10.0) as u8
}

/// "0%" … "100%"; the step nearest to `volume` is checked.
pub fn volume_entries(lang: &str, volume: f32) -> Vec<CheckEntry> {
    let current = volume_step(volume);
    (0..=100u8)
        .step_by(10)
        .map(|p| CheckEntry {
            id: format!("{VOLUME_ID_PREFIX}{p}"),
            label: trf(lang, "tray.volume_step", &[("n", &p.to_string())]),
            checked: p == current,
        })
        .collect()
}

/// The volume (0..1) a "Volume" item sets; `None` for other items.
pub fn volume_pick(menu_id: &str) -> Option<f32> {
    let p: u8 = menu_id.strip_prefix(VOLUME_ID_PREFIX)?.parse().ok()?;
    (p <= 100).then(|| f32::from(p) / 100.0)
}

/// "System default", then every output device; a saved device that is not
/// connected stays at the end, marked so. The current one is checked.
pub fn output_entries(lang: &str, devices: &[String], current: Option<&str>) -> Vec<CheckEntry> {
    let mut entries = vec![CheckEntry {
        id: OUTPUT_DEFAULT_ID.into(),
        label: tr(lang, "settings.device_default"),
        checked: current.is_none(),
    }];
    let device = |name: &str, label: String| CheckEntry {
        id: format!("{OUTPUT_ID_PREFIX}{name}"),
        label,
        checked: current == Some(name),
    };
    entries.extend(devices.iter().map(|d| device(d, d.clone())));
    if let Some(saved) = current.filter(|c| !devices.iter().any(|d| d == c)) {
        let label = trf(lang, "settings.device_missing", &[("name", saved)]);
        entries.push(device(saved, label));
    }
    entries
}

/// The device an "Output" item selects (`Some(None)`: system default); `None` for
/// other items.
pub fn output_pick(menu_id: &str) -> Option<Option<String>> {
    if menu_id == OUTPUT_DEFAULT_ID {
        return Some(None);
    }
    menu_id
        .strip_prefix(OUTPUT_ID_PREFIX)
        .map(|name| Some(name.to_string()))
}

/// The genre a menu item id stands for; `None` for other items.
pub fn genre_pick(menu_id: &str) -> Option<GenrePick> {
    if menu_id == ALL_GENRES_ID {
        return Some(GenrePick::All);
    }
    menu_id
        .strip_prefix(GENRE_ID_PREFIX)
        .map(|key| GenrePick::One(key.to_string()))
}

/// "Artist – Title (1991)" for the menu and the tooltip; a long label is shortened
/// before the year.
pub fn track_label(lang: &str, np: Option<&NowPlaying>) -> String {
    let parts: Vec<&str> = np
        .map(|n| [n.artist.trim(), n.title.trim()])
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return tr(lang, "tray.nothing_playing");
    }
    let label = parts.join(" – ");
    let year = np
        .and_then(|n| n.year)
        .map(|y| format!(" ({y})"))
        .unwrap_or_default();
    let room = LABEL_MAX_CHARS - year.chars().count();
    if label.chars().count() <= room {
        return label + &year;
    }
    let mut short: String = label.chars().take(room - 1).collect();
    short.push('…');
    short + &year
}

/// Menu item text as shown: macOS menus drop a single `&` (mnemonic marker).
pub fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

pub fn pause_label(lang: &str, np: Option<&NowPlaying>) -> String {
    let key = if np.is_some_and(|n| n.paused) {
        "player.resume"
    } else {
        "player.pause"
    };
    tr(lang, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn genre(key: &str, count: usize) -> GenreCount {
        GenreCount {
            key: key.into(),
            display: key.to_uppercase(),
            count,
        }
    }

    fn np(artist: &str, title: &str, paused: bool) -> NowPlaying {
        NowPlaying {
            duration_ms: None,
            id: 1,
            artist: artist.into(),
            title: title.into(),
            album: String::new(),
            year: None,
            genre_display: String::new(),
            paused,
        }
    }

    fn with_year(mut n: NowPlaying, year: u16) -> NowPlaying {
        n.year = Some(year);
        n
    }

    #[test]
    fn track_label_ends_with_the_year() {
        assert_eq!(
            track_label("en", Some(&with_year(np("Artist", "Song", false), 1991))),
            "Artist – Song (1991)"
        );
        assert_eq!(
            track_label("en", Some(&with_year(np("", "Song", false), 2003))),
            "Song (2003)"
        );
        // Shortened before the year, which stays.
        let long = "ž".repeat(100);
        let label = track_label("en", Some(&with_year(np("A", &long, false), 1991)));
        assert_eq!(label.chars().count(), LABEL_MAX_CHARS);
        assert!(label.ends_with("… (1991)"), "{label}");
    }

    #[test]
    fn genre_menu_starts_with_all_and_orders_by_count() {
        let genres = [genre("a", 5), genre("b", 50), genre("c", 7)];
        let e = genre_entries("en", &genres, &[]);
        let labels: Vec<_> = e.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["All", "B", "C", "A"]);
        assert_eq!(genre_pick(&e[0].id), Some(GenrePick::All));
        assert_eq!(genre_pick(&e[1].id), Some(GenrePick::One("b".into())));
    }

    #[test]
    fn genre_menu_keeps_top_30_plus_last_selection() {
        // g0 has the most tracks, g39 the fewest.
        let genres: Vec<_> = (0..40).map(|i| genre(&format!("g{i}"), 100 - i)).collect();
        let last = ["g35".to_string(), "g2".into(), "gone".into(), "g31".into()];
        let e = genre_entries("en", &genres, &last);
        assert_eq!(e.len(), 1 + TOP_GENRES + 2);
        assert_eq!(e[1].label, "G0");
        assert_eq!(e[TOP_GENRES].label, "G29");
        // Extras that are not in the top 30, by count; unknown keys are left out.
        assert_eq!(e[TOP_GENRES + 1].label, "G31");
        assert_eq!(e[TOP_GENRES + 2].label, "G35");
        let ids: std::collections::HashSet<_> = e.iter().map(|e| &e.id).collect();
        assert_eq!(ids.len(), e.len(), "ids are unique");
    }

    #[test]
    fn genre_menu_ties_sort_by_name_and_empty_catalog_has_only_all() {
        let genres = [genre("zz", 3), genre("aa", 3)];
        let labels: Vec<_> = genre_entries("en", &genres, &[])
            .into_iter()
            .map(|e| e.label)
            .collect();
        assert_eq!(labels, ["All", "AA", "ZZ"]);
        assert_eq!(genre_entries("en", &[], &["x".into()]).len(), 1);
    }

    #[test]
    fn genre_pick_ignores_other_items_and_keeps_odd_keys() {
        assert_eq!(genre_pick("next"), None);
        assert_eq!(
            genre_pick("genre:drum & bass: live"),
            Some(GenrePick::One("drum & bass: live".into()))
        );
    }

    #[test]
    fn track_label_joins_artist_and_title() {
        assert_eq!(
            track_label("en", Some(&np("Artist", "Song", false))),
            "Artist – Song"
        );
        assert_eq!(track_label("en", Some(&np(" ", "Song", false))), "Song");
        assert_eq!(track_label("en", Some(&np("Artist", "", false))), "Artist");
        assert_eq!(track_label("en", None), "Nothing playing");
    }

    #[test]
    fn track_label_is_shortened_for_the_menu() {
        let long = "ž".repeat(100);
        let label = track_label("en", Some(&np("A", &long, false)));
        assert_eq!(label.chars().count(), LABEL_MAX_CHARS);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn menu_text_keeps_ampersands() {
        assert_eq!(menu_text("Drum & Bass"), "Drum && Bass");
        assert_eq!(menu_text("A&&B"), "A&&&&B");
        assert_eq!(menu_text("Plain"), "Plain");
    }

    #[test]
    fn pause_item_says_what_it_does() {
        assert_eq!(pause_label("en", Some(&np("A", "B", false))), "Pause");
        assert_eq!(pause_label("en", Some(&np("A", "B", true))), "Resume");
        assert_eq!(pause_label("en", None), "Pause");
        assert_eq!(pause_label("hr", Some(&np("A", "B", true))), "Nastavi");
        assert_eq!(track_label("hr", None), "Ništa ne svira");
    }

    #[test]
    fn genre_menu_labels_all_and_unknown_in_the_language() {
        let genres = [genre("_none", 5), genre("jazz", 3)];
        let labels = |lang| -> Vec<String> {
            genre_entries(lang, &genres, &[])
                .into_iter()
                .map(|e| e.label)
                .collect()
        };
        assert_eq!(labels("en"), ["All", "Unknown", "JAZZ"]);
        assert_eq!(labels("hr"), ["Sve", "Nepoznato", "JAZZ"]);
    }

    #[test]
    fn volume_menu_checks_the_nearest_ten() {
        let e = volume_entries("en", 0.64);
        assert_eq!(e.len(), 11);
        assert_eq!(e[0].label, "0%");
        assert_eq!(e[10].label, "100%");
        let checked: Vec<_> = e
            .iter()
            .filter(|e| e.checked)
            .map(|e| e.label.as_str())
            .collect();
        assert_eq!(checked, ["60%"]);
        assert_eq!(volume_step(0.65), 70);
        assert_eq!(volume_step(0.04), 0);
        assert_eq!(volume_step(1.0), 100);
        assert_eq!(volume_step(f32::NAN), 0);
        assert_eq!(volume_entries("hr", 0.5)[5].label, "50 %");
    }

    #[test]
    fn volume_items_set_the_volume_they_show() {
        for e in volume_entries("en", 0.3) {
            let v = volume_pick(&e.id).expect(&e.id);
            assert_eq!(volume_step(v).to_string() + "%", e.label);
        }
        assert_eq!(volume_pick("vol:110"), None);
        assert_eq!(volume_pick("next"), None);
    }

    #[test]
    fn output_menu_lists_the_default_and_every_device() {
        let devices = ["MacBook Speakers".to_string(), "Rock & Roll DAC".into()];
        let e = output_entries("en", &devices, None);
        let labels: Vec<_> = e.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            ["System default", "MacBook Speakers", "Rock & Roll DAC"]
        );
        assert_eq!(e.iter().filter(|e| e.checked).count(), 1);
        assert!(e[0].checked);
        for (entry, want) in e
            .iter()
            .zip([None, Some("MacBook Speakers"), Some("Rock & Roll DAC")])
        {
            assert_eq!(output_pick(&entry.id), Some(want.map(String::from)));
        }
        let e = output_entries("hr", &devices, Some("Rock & Roll DAC"));
        assert_eq!(e[0].label, "Zadani uređaj sustava");
        assert!(e[2].checked && !e[0].checked && !e[1].checked);
        assert_eq!(output_pick("vol:10"), None);
    }

    #[test]
    fn output_menu_keeps_a_saved_device_that_is_not_connected() {
        let devices = ["Speakers".to_string()];
        let e = output_entries("en", &devices, Some("USB DAC"));
        let labels: Vec<_> = e.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            ["System default", "Speakers", "USB DAC (not connected)"]
        );
        assert!(e[2].checked);
        assert_eq!(output_pick(&e[2].id), Some(Some("USB DAC".into())));
    }
}
