//! Menu bar (tray) icon and its menu.

use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::time::Duration;

use smp_core::audio::player::Command;
use smp_core::catalog::GenreCount;
use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Listener, Manager, Wry};

use crate::commands::{ban_current_now, play_genres_now, spawn_action};
use crate::state::{self, lock, AppState, NowPlaying};

/// Genres change in bursts (every few seconds during a scan): the submenu is rebuilt
/// once they settle for this long.
const GENRE_MENU_DEBOUNCE: Duration = Duration::from_secs(1);
const TOOLTIP_IDLE: &str = "SMB Music Player";
const ID_TOGGLE: &str = "toggle";
const ID_NEXT: &str = "next";
const ID_BAN: &str = "ban";
const ID_SHOW: &str = "show";
const ID_QUIT: &str = "quit";

/// Genres in the "Žanr" submenu besides "Sve" and the last selection.
pub const TOP_GENRES: usize = 30;
const ALL_GENRES_ID: &str = "genre-all";
const GENRE_ID_PREFIX: &str = "genre:";
/// Longest track label in the menu, in characters.
const LABEL_MAX_CHARS: usize = 60;

/// One item of the "Žanr" submenu.
#[derive(Debug, Clone, PartialEq)]
pub struct GenreEntry {
    pub id: String,
    pub label: String,
}

/// What a "Žanr" submenu item plays.
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
    ban: MenuItem<Wry>,
    genres: Submenu<Wry>,
    rebuild_genres: Mutex<Sender<()>>,
}

/// Creates the tray icon. Runs on the main thread, after `AppState::init`.
pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let now = MenuItem::with_id(app, "now", track_label(None), false, None::<&str>)?;
    let pause = MenuItem::with_id(app, ID_TOGGLE, pause_label(None), true, None::<&str>)?;
    let next = MenuItem::with_id(app, ID_NEXT, "Sljedeća", true, None::<&str>)?;
    let ban = MenuItem::with_id(app, ID_BAN, "Nikad više ovu pjesmu", false, None::<&str>)?;
    // Filled in the background: the catalog is not queried on the main thread.
    let genres = Submenu::with_items(app, "Žanr", true, &[])?;
    let show = MenuItem::with_id(app, ID_SHOW, "Prikaži prozor", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Izlaz", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &now,
            &PredefinedMenuItem::separator(app)?,
            &pause,
            &next,
            &ban,
            &PredefinedMenuItem::separator(app)?,
            &genres,
            &PredefinedMenuItem::separator(app)?,
            &show,
            &quit,
        ],
    )?;
    let icon = TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(include_bytes!(
            "../icons/tray-next@2x.png"
        ))?)
        .icon_as_template(true)
        .tooltip(TOOLTIP_IDLE)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                log::debug!("tray: left click, next track");
                tray.app_handle()
                    .state::<AppState>()
                    .player
                    .send(Command::Next);
            }
        })
        .build(app)?;

    let (tx, rx) = mpsc::channel();
    app.manage(Tray {
        icon,
        now,
        pause,
        ban,
        genres,
        rebuild_genres: Mutex::new(tx),
    });

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
    state::on_now_playing(app, move |np| show_now_playing(&handle, np.as_ref()));
    Ok(())
}

/// Schedules a rebuild of the "Žanr" submenu (no-op before the tray exists).
pub fn refresh_genres(app: &AppHandle) {
    if let Some(tray) = app.try_state::<Tray>() {
        let _ = lock(&tray.rebuild_genres).send(());
    }
}

/// Replaces the "Žanr" items unless they would stay the same as `shown`.
fn rebuild_genre_menu(app: &AppHandle, shown: &mut Vec<GenreEntry>) -> Result<(), String> {
    let st = app.state::<AppState>();
    let genres = lock(&st.query).genres().map_err(|e| e.to_string())?;
    let entries = genre_entries(&genres, &st.settings.get().last_selection);
    if entries == *shown {
        return Ok(());
    }
    let menu = &app.state::<Tray>().genres;
    let err = |e: tauri::Error| e.to_string();
    for item in menu.items().map_err(err)? {
        menu.remove(&item).map_err(err)?;
    }
    for e in &entries {
        let item = MenuItem::with_id(app, &e.id, &e.label, true, None::<&str>).map_err(err)?;
        menu.append(&item).map_err(err)?;
    }
    log::info!("tray genre menu: {} items", entries.len());
    *shown = entries;
    Ok(())
}

fn show_now_playing(app: &AppHandle, np: Option<&NowPlaying>) {
    let Some(tray) = app.try_state::<Tray>() else {
        return;
    };
    let label = track_label(np);
    let _ = tray.now.set_text(&label);
    let _ = tray.pause.set_text(pause_label(np));
    let _ = tray.ban.set_enabled(np.is_some());
    let tooltip = if np.is_some() {
        label.as_str()
    } else {
        TOOLTIP_IDLE
    };
    let _ = tray.icon.set_tooltip(Some(tooltip));
}

/// Runs on the main thread: anything that touches the catalog goes to a thread.
fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref();
    let st = app.state::<AppState>();
    match id {
        ID_TOGGLE => st.player.send(Command::TogglePause),
        ID_NEXT => st.player.send(Command::Next),
        ID_BAN => spawn_action(app, "ban", |_, st| ban_current_now(st)),
        ID_SHOW => show_main_window(app),
        ID_QUIT => app.exit(0),
        _ => {
            if let Some(pick) = genre_pick(id) {
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

/// Shows, restores and focuses the main window.
pub fn show_main_window(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let _ = w.show();
    let _ = w.unminimize();
    let _ = w.set_focus();
}

/// "Sve", then the [`TOP_GENRES`] largest genres by track count, then the genres of
/// `last_selection` that are not among them.
pub fn genre_entries(genres: &[GenreCount], last_selection: &[String]) -> Vec<GenreEntry> {
    let mut by_count: Vec<&GenreCount> = genres.iter().collect();
    by_count.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
            .then_with(|| a.key.cmp(&b.key))
    });
    let entry = |g: &GenreCount| GenreEntry {
        id: format!("{GENRE_ID_PREFIX}{}", g.key),
        label: g.display.clone(),
    };
    let mut entries = vec![GenreEntry {
        id: ALL_GENRES_ID.into(),
        label: "Sve".into(),
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

/// The genre a menu item id stands for; `None` for other items.
pub fn genre_pick(menu_id: &str) -> Option<GenrePick> {
    if menu_id == ALL_GENRES_ID {
        return Some(GenrePick::All);
    }
    menu_id
        .strip_prefix(GENRE_ID_PREFIX)
        .map(|key| GenrePick::One(key.to_string()))
}

/// "Izvođač – Naslov" for the menu and the tooltip.
pub fn track_label(np: Option<&NowPlaying>) -> String {
    let parts: Vec<&str> = np
        .map(|n| [n.artist.trim(), n.title.trim()])
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return "Ništa ne svira".into();
    }
    let label = parts.join(" – ");
    if label.chars().count() <= LABEL_MAX_CHARS {
        return label;
    }
    let mut short: String = label.chars().take(LABEL_MAX_CHARS - 1).collect();
    short.push('…');
    short
}

pub fn pause_label(np: Option<&NowPlaying>) -> &'static str {
    if np.is_some_and(|n| n.paused) {
        "Nastavi"
    } else {
        "Pauza"
    }
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
            id: 1,
            artist: artist.into(),
            title: title.into(),
            album: String::new(),
            genre_display: String::new(),
            paused,
        }
    }

    #[test]
    fn genre_menu_starts_with_all_and_orders_by_count() {
        let genres = [genre("a", 5), genre("b", 50), genre("c", 7)];
        let e = genre_entries(&genres, &[]);
        let labels: Vec<_> = e.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["Sve", "B", "C", "A"]);
        assert_eq!(genre_pick(&e[0].id), Some(GenrePick::All));
        assert_eq!(genre_pick(&e[1].id), Some(GenrePick::One("b".into())));
    }

    #[test]
    fn genre_menu_keeps_top_30_plus_last_selection() {
        // g0 has the most tracks, g39 the fewest.
        let genres: Vec<_> = (0..40).map(|i| genre(&format!("g{i}"), 100 - i)).collect();
        let last = ["g35".to_string(), "g2".into(), "gone".into(), "g31".into()];
        let e = genre_entries(&genres, &last);
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
        let labels: Vec<_> = genre_entries(&genres, &[])
            .into_iter()
            .map(|e| e.label)
            .collect();
        assert_eq!(labels, ["Sve", "AA", "ZZ"]);
        assert_eq!(genre_entries(&[], &["x".into()]).len(), 1);
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
            track_label(Some(&np("Artist", "Song", false))),
            "Artist – Song"
        );
        assert_eq!(track_label(Some(&np(" ", "Song", false))), "Song");
        assert_eq!(track_label(Some(&np("Artist", "", false))), "Artist");
        assert_eq!(track_label(None), "Ništa ne svira");
    }

    #[test]
    fn track_label_is_shortened_for_the_menu() {
        let long = "ž".repeat(100);
        let label = track_label(Some(&np("A", &long, false)));
        assert_eq!(label.chars().count(), LABEL_MAX_CHARS);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn pause_item_says_what_it_does() {
        assert_eq!(pause_label(Some(&np("A", "B", false))), "Pauza");
        assert_eq!(pause_label(Some(&np("A", "B", true))), "Nastavi");
        assert_eq!(pause_label(None), "Pauza");
    }
}
