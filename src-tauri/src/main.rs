// Prevents an additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod commands;
mod connection;
mod credentials;
mod devdata;
mod i18n;
mod lookup;
mod media;
mod playlist;
mod provider;
mod settings;
mod state;
mod tray;
mod writer;

use tauri::{Manager, WindowEvent};

/// Default log filter (`RUST_LOG` overrides it). The `smb` crate is noisy at info,
/// but its directive also prefix-matches this crate, so ours is named explicitly.
const LOG_FILTER: &str = "info,smb=warn,smb_music_player=info";

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(LOG_FILTER)).init();
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
        .plugin(autostart::plugin())
        .setup(|app| {
            // Menu bar app: no Dock icon.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
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
            let configured = !settings.primary.username.trim().is_empty();
            if !(configured && autostart::is_autostart_launch(std::env::args())) {
                tray::show_main_window(handle);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing only hides the window; the music and the tray icon stay.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::save_settings,
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
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(st) = app.try_state::<state::AppState>() {
                    st.save_playlist_now();
                    if let Err(e) = st.settings.flush() {
                        log::warn!("cannot save settings on exit: {e}");
                    }
                }
            }
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
        assert!(enabled("smb_music_player::state", Level::Info));
        assert!(enabled("smb_music_player", Level::Info));
        assert!(enabled("smp_core::scan", Level::Info));
        assert!(!enabled("smb::session::setup", Level::Info));
        assert!(enabled("smb::session::setup", Level::Warn));
    }
}
