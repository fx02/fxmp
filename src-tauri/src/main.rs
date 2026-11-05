// Prevents an additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod about;
mod autostart;
mod commands;
mod connection;
mod credentials;
mod devdata;
mod i18n;
mod logfile;
mod lookup;
mod media;
mod migrate;
mod playlist;
mod portable;
mod provider;
mod settings;
mod state;
mod tray;
mod tray_click;
mod writer;

use tauri::{Manager, WindowEvent};

/// Default log filter (`RUST_LOG` overrides it). The `smb` crate is noisy at info.
const LOG_FILTER: &str = "info,smb=warn";

fn main() {
    let log_sink = logfile::LogSink::default();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(LOG_FILTER))
        .target(env_logger::Target::Pipe(Box::new(log_sink.clone())))
        .init();
    let mut builder = tauri::Builder::default();
    // First, so a second launch exits before anything else starts. A debug build
    // with its own data folder runs beside the installed app instead of handing
    // over to it.
    if devdata::current().is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if !autostart::is_autostart_launch(&args) {
                tray::show_main_window(app);
            }
        }));
    }
    builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(autostart::plugin())
        .setup(move |app| {
            let log_dir = match devdata::current().or_else(portable::current) {
                Some(dir) => Ok(dir.join("logs")),
                None => app.path().app_log_dir(),
            };
            match log_dir
                .map_err(|e| e.to_string())
                .and_then(|d| log_sink.attach(&d).map_err(|e| e.to_string()))
            {
                Ok(path) => {
                    log::info!("fxmp {} log: {}", env!("CARGO_PKG_VERSION"), path.display())
                }
                Err(e) => log::warn!("no log file: {e}"),
            }
            // In the Dock and the menu bar: the Dock icon opens the window, Cmd+Q
            // quits (the default app menu), the tray icon stays.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Regular);
            let handle = app.handle();
            state::AppState::init(handle)?;
            tray::init(handle)?;
            media::init(handle);
            let st = handle.state::<state::AppState>();
            let settings = st.settings.get();
            if let Err(e) = autostart::apply(handle, settings.autostart, true) {
                log::warn!("start at login: {e}");
            }
            // The window starts hidden: a start at login stays in the menu bar unless
            // the server still has to be set up.
            let configured =
                !settings.primary.username.trim().is_empty() || !settings.libraries.is_empty();
            if !(configured && autostart::is_autostart_launch(std::env::args())) {
                tray::show_main_window(handle);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing only hides the window; the music, the Dock and the tray icon
            // stay. Minimizing is left to the system (the window goes to the Dock).
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::save_settings,
            commands::get_libraries,
            commands::save_library,
            commands::set_library_enabled,
            commands::remove_library,
            commands::test_folder,
            commands::pick_folder,
            commands::test_connection,
            commands::get_connection_status,
            commands::start_scan,
            commands::cancel_scan,
            commands::get_scan_status,
            commands::get_genres,
            commands::play_genres,
            commands::play_album,
            commands::toggle_pause,
            commands::next,
            commands::set_volume,
            commands::seek,
            commands::set_fill_speakers,
            commands::list_devices,
            commands::set_device,
            commands::set_crossfade,
            commands::set_normalize,
            commands::ban_current,
            commands::list_banned,
            commands::unban,
            commands::merge_genre,
            commands::known_genres,
            commands::get_locale,
            commands::list_languages,
            commands::set_language,
            commands::list_artists,
            commands::list_albums,
            commands::now_playing,
            commands::list_unreadable,
            commands::unreadable_count,
            commands::retry_unreadable,
            commands::reset_settings,
            commands::clear_library,
            commands::previous,
            commands::get_position,
            commands::get_playlist,
            commands::playlist_facets,
            commands::playlist_jump,
            commands::playlist_add,
            commands::playlist_add_album,
            commands::playlist_remove,
            commands::playlist_move,
            commands::playlist_new,
            commands::playlist_play_filtered,
            commands::playlist_export_hint,
            commands::list_album_tracks,
            commands::set_resume_autoplay,
            commands::playlist_export_save,
            commands::lookup_services,
            commands::lookup_track,
            commands::lookup_artist,
            commands::lookup_current,
            commands::current_song_text,
            commands::set_lookup_service,
            commands::app_about,
            commands::open_project_page,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            tauri::RunEvent::Exit => {
                if let Some(st) = app.try_state::<state::AppState>() {
                    st.save_playlist_now();
                    if let Err(e) = st.settings.flush() {
                        log::warn!("cannot save settings on exit: {e}");
                    }
                }
            }
            // A click on the Dock icon (also with the window hidden or minimized).
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => tray::show_main_window(app),
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use log::{Level, Log, Metadata};

    fn enabled(target: &str, level: Level) -> bool {
        let logger = env_logger::Builder::new()
            .parse_filters(super::LOG_FILTER)
            .build();
        logger.enabled(&Metadata::builder().target(target).level(level).build())
    }

    #[test]
    fn log_filter_shows_app_info_but_keeps_smb_at_warn() {
        assert!(enabled("fxmp::state", Level::Info));
        assert!(enabled("fxmp", Level::Info));
        assert!(enabled("fxmp_core::scan", Level::Info));
        assert!(!enabled("smb::session::setup", Level::Info));
        assert!(enabled("smb::session::setup", Level::Warn));
    }
}
