// Prevents an additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod connection;
mod credentials;
mod provider;
mod settings;
mod state;
mod writer;

use tauri::Manager;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,smb=warn"))
        .init();
    tauri::Builder::default()
        .setup(|app| {
            state::AppState::init(app.handle())?;
            Ok(())
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
            commands::list_artists,
            commands::list_albums,
            commands::now_playing,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(st) = app.try_state::<state::AppState>() {
                    if let Err(e) = st.settings.flush() {
                        log::warn!("cannot save settings on exit: {e}");
                    }
                }
            }
        });
}
