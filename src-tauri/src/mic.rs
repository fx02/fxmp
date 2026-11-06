//! "Microphone in use": lowers or pauses the music while some app uses the
//! microphone (a call, a meeting), or asks which (a notification and a bar in the
//! window), and undoes it once the microphone is free.
//! Nothing is recorded and no microphone permission is needed: the system is only
//! asked whether its input is in use (macOS: the default input device is running,
//! the signal of the orange dot; Windows: the microphone consent store, the signal
//! of its microphone icon).

use std::time::{Duration, Instant};

use fxmp_core::audio::player::Command;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::commands::{pause_now, play_pause_now};
use crate::i18n;
use crate::state::{lock, AppState};

/// What happens while the microphone is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MicMode {
    Off,
    /// Asks: Lower, Pause or Ignore (see [`respond`]).
    Ask,
    /// Down to the set level (`mic_level` %) of the volume.
    #[default]
    Lower,
    Pause,
}

/// The volume while lowered, in % of the set one, when not set.
pub const DEFAULT_LEVEL: u8 = 20;
/// The highest level that can be set (more is hardly lower).
pub const MAX_LEVEL: u8 = 80;
/// The microphone must stay free this long before the music comes back, so a
/// short mute does not pump the volume.
pub const RELEASE: Duration = Duration::from_secs(3);
/// How often the microphone is checked.
const POLL: Duration = Duration::from_secs(1);

/// What to do to the music.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Lower,
    Restore,
    Pause,
    Resume,
    /// Ask what to do (the notification and the bar).
    Ask,
    /// The question is moot (the microphone is free again): hide the bar.
    Withdraw,
}

/// The lowering and pausing this module did, so only that is undone.
#[derive(Debug, Default)]
pub struct Ducker {
    lowered: bool,
    /// Paused by us (never set for music already paused).
    paused: bool,
    /// Played again by hand during this use of the microphone: not paused again
    /// until it is free.
    overridden: bool,
    /// Asked during this use of the microphone (not asked again until it is free).
    asked: bool,
    /// The question is open: the bar shows and an answer counts.
    open: bool,
    free_since: Option<Instant>,
}

impl Ducker {
    /// One check: the mode, whether the microphone is in use, whether music plays.
    pub fn step(&mut self, mode: MicMode, mic: bool, playing: bool, now: Instant) -> Vec<Act> {
        let mut acts = Vec::new();
        // Played again by hand during the call: the pause is no longer ours.
        if self.paused && playing {
            self.paused = false;
            self.overridden = true;
        }
        if mode == MicMode::Off {
            if std::mem::take(&mut self.lowered) {
                acts.push(Act::Restore);
            }
            self.asked = false;
            if std::mem::take(&mut self.open) {
                acts.push(Act::Withdraw);
            }
            self.paused = false;
            self.free_since = None;
            return acts;
        }
        if mic {
            self.free_since = None;
            match mode {
                MicMode::Lower if !self.lowered => {
                    self.lowered = true;
                    acts.push(Act::Lower);
                }
                MicMode::Pause if playing && !self.paused && !self.overridden => {
                    self.paused = true;
                    acts.push(Act::Pause);
                }
                MicMode::Ask if playing && !self.asked && !self.lowered && !self.paused => {
                    self.asked = true;
                    self.open = true;
                    acts.push(Act::Ask);
                }
                _ => {}
            }
            return acts;
        }
        self.overridden = false;
        self.asked = false;
        if std::mem::take(&mut self.open) {
            acts.push(Act::Withdraw);
        }
        if !self.lowered && !self.paused {
            self.free_since = None;
            return acts;
        }
        let since = *self.free_since.get_or_insert(now);
        if now.duration_since(since) < RELEASE {
            return acts;
        }
        self.free_since = None;
        if std::mem::take(&mut self.lowered) {
            acts.push(Act::Restore);
        }
        if std::mem::take(&mut self.paused) && !playing {
            acts.push(Act::Resume);
        }
        acts
    }

    /// The answer to [`Act::Ask`]: what to do now (undone once the microphone is
    /// free, as in the other modes).
    pub fn answer(&mut self, answer: Answer, playing: bool) -> Option<Act> {
        if !std::mem::take(&mut self.open) {
            return None;
        }
        match answer {
            Answer::Lower if !self.lowered => {
                self.lowered = true;
                Some(Act::Lower)
            }
            Answer::Pause if playing => {
                self.paused = true;
                Some(Act::Pause)
            }
            _ => None,
        }
    }

    /// Something to undo once the microphone is free.
    pub fn active(&self) -> bool {
        self.lowered || self.paused || self.open
    }
}

/// The answer to the question of [`MicMode::Ask`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Lower,
    Pause,
    Ignore,
}

/// Shared by the watching thread and the window's answer.
static DUCKER: std::sync::Mutex<Ducker> = std::sync::Mutex::new(Ducker {
    lowered: false,
    paused: false,
    overridden: false,
    asked: false,
    open: false,
    free_since: None,
});

/// The volume share while lowered, from the `mic_level` setting (in %).
pub fn level(percent: u8) -> f32 {
    f32::from(percent.min(MAX_LEVEL)) / 100.0
}

fn playing(st: &AppState) -> bool {
    lock(&st.engine).loaded() && !st.paused.load(std::sync::atomic::Ordering::SeqCst)
}

fn apply(app: &AppHandle, st: &AppState, act: Act) {
    match act {
        Act::Lower => st
            .player
            .send(Command::SetDuck(level(st.settings.get().mic_level))),
        Act::Restore => st.player.send(Command::SetDuck(1.0)),
        Act::Pause => pause_now(st),
        Act::Resume => play_pause_now(app, st, Command::Resume),
        Act::Ask => {
            let _ = app.emit("mic-ask", true);
            let shown = app
                .notification()
                .builder()
                .title(i18n::t("mic.ask_title"))
                .body(i18n::t("mic.ask_body"))
                .show();
            if let Err(e) = shown {
                log::warn!("notification: {e}");
            }
        }
        Act::Withdraw => {
            let _ = app.emit("mic-ask", false);
        }
    }
}

/// A new level while lowered applies at once.
pub fn level_changed(st: &AppState, percent: u8) {
    if lock(&DUCKER).lowered {
        st.player.send(Command::SetDuck(level(percent)));
    }
}

/// The window's answer (the bar's Lower / Pause / Ignore).
pub fn respond(app: &AppHandle, answer: Answer) {
    let st = app.state::<AppState>();
    let act = lock(&DUCKER).answer(answer, playing(&st));
    let _ = app.emit("mic-ask", false);
    if let Some(act) = act {
        log::info!("microphone: answered {answer:?}");
        apply(app, &st, act);
    }
}

/// Checks the microphone in the background for the app's lifetime.
pub fn start(app: &AppHandle) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("mic-watch".into())
        .spawn(move || {
            let mut unknown_logged = false;
            loop {
                std::thread::sleep(POLL);
                let st = app.state::<AppState>();
                let settings = st.settings.get();
                let mode = settings.mic_duck;
                if mode == MicMode::Off && !lock(&DUCKER).active() {
                    continue;
                }
                let mic = match in_use(&app_filter(&settings.mic_apps)) {
                    Some(m) => m,
                    None => {
                        if !unknown_logged {
                            log::info!("microphone use cannot be told here");
                            unknown_logged = true;
                        }
                        false
                    }
                };
                let acts = lock(&DUCKER).step(mode, mic, playing(&st), Instant::now());
                for act in acts {
                    log::info!(
                        "microphone {}: {act:?}",
                        if mic { "in use" } else { "free" }
                    );
                    apply(&app, &st, act);
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("cannot watch the microphone: {e}");
    }
}

/// The `mic_apps` setting as lowercase name parts ("teams, Zoom" → teams, zoom);
/// empty: any app.
pub fn app_filter(setting: &str) -> Vec<String> {
    setting
        .split([',', ';', '\n'])
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether an app whose consent-store key is `key` counts (Windows: a packaged
/// app's name, or a desktop app's path with `#` for `\`).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn app_counts(key: &str, filter: &[String]) -> bool {
    let key = key.to_lowercase();
    filter.is_empty() || filter.iter().any(|f| key.contains(f.as_str()))
}

/// Whether some app is using the microphone (`None`: cannot tell). macOS 14.2+
/// says for each process whether it is recording, from whichever input device
/// (a call often uses the headset's microphone, not the default input); older
/// systems only say whether the default input device is running.
/// macOS cannot tell which app it is: `_apps` is not used.
#[cfg(target_os = "macos")]
pub fn in_use(_apps: &[String]) -> Option<bool> {
    mac::any_process_recording().or_else(mac::default_input_running)
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2_core_audio::{
        kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioHardwarePropertyDefaultInputDevice,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyProcessObjectList,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
        kAudioProcessPropertyIsRunningInput, kAudioProcessPropertyPID, AudioObjectGetPropertyData,
        AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
    };
    use std::ptr::NonNull;

    const SYSTEM: u32 = kAudioObjectSystemObject as u32;

    fn address(selector: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    /// A property of fixed size `T` (a u32 or an i32).
    fn get<T: Default + Copy>(object: u32, selector: u32) -> Option<T> {
        let address = address(selector);
        let mut value = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        // SAFETY: valid pointers to a property address, a size and a `T` that
        // outlive the call; the size tells CoreAudio how much it may write.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            )
        };
        (status == 0 && size as usize == std::mem::size_of::<T>()).then_some(value)
    }

    /// A property that is a list of audio object ids.
    fn get_list(object: u32, selector: u32) -> Option<Vec<u32>> {
        let address = address(selector);
        let mut size = 0u32;
        // SAFETY: as in `get`; this only reads the size of the list.
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                object,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
            )
        };
        if status != 0 {
            return None;
        }
        let mut ids = vec![0u32; size as usize / std::mem::size_of::<u32>()];
        if ids.is_empty() {
            return Some(ids);
        }
        // SAFETY: `ids` has room for `size` bytes, which CoreAudio may not exceed.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(ids.as_mut_ptr()).expect("non-empty").cast(),
            )
        };
        if status != 0 {
            return None;
        }
        ids.truncate(size as usize / std::mem::size_of::<u32>());
        Some(ids)
    }

    /// Some other process records from any input device (`None`: this macOS
    /// does not list audio processes, before 14.2).
    pub fn any_process_recording() -> Option<bool> {
        let own = std::process::id() as i32;
        let processes = get_list(SYSTEM, kAudioHardwarePropertyProcessObjectList)?;
        Some(processes.into_iter().any(|p| {
            get::<u32>(p, kAudioProcessPropertyIsRunningInput) == Some(1)
                && get::<i32>(p, kAudioProcessPropertyPID) != Some(own)
        }))
    }

    /// The default input device runs. A headset that is also the default output
    /// runs while music plays: its "running" says nothing, so `None` then.
    pub fn default_input_running() -> Option<bool> {
        let input = get::<u32>(SYSTEM, kAudioHardwarePropertyDefaultInputDevice)?;
        if input == 0 {
            return Some(false);
        }
        if get::<u32>(SYSTEM, kAudioHardwarePropertyDefaultOutputDevice) == Some(input) {
            return None;
        }
        get::<u32>(input, kAudioDevicePropertyDeviceIsRunningSomewhere).map(|v| v != 0)
    }
}

/// Whether some app is using the microphone (`None`: cannot tell): an app in the
/// microphone consent store that started using it and has not stopped.
#[cfg(target_os = "windows")]
pub fn in_use(apps: &[String]) -> Option<bool> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const STORE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

    let any_active = |key: &RegKey| -> bool {
        key.enum_keys().flatten().any(|name| {
            app_counts(&name, apps)
                && key.open_subkey(&name).is_ok_and(|app| {
                    let start: u64 = app.get_value("LastUsedTimeStart").unwrap_or(0);
                    let stop: u64 = app.get_value("LastUsedTimeStop").unwrap_or(1);
                    start > 0 && stop == 0
                })
        })
    };

    let store = RegKey::predef(HKEY_CURRENT_USER).open_subkey(STORE).ok()?;
    // Store apps under the key itself, desktop apps (Zoom, Teams…) under NonPackaged.
    Some(
        any_active(&store)
            || store
                .open_subkey("NonPackaged")
                .is_ok_and(|k| any_active(&k)),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn in_use(_apps: &[String]) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn lowers_during_a_call_and_restores_after_it() {
        let (t0, mut d) = (Instant::now(), Ducker::default());
        assert_eq!(d.step(MicMode::Lower, false, true, t0), []);
        assert_eq!(
            d.step(MicMode::Lower, true, true, at(t0, 1000)),
            [Act::Lower]
        );
        assert_eq!(d.step(MicMode::Lower, true, true, at(t0, 2000)), []);
        // A short mute does not bring the volume back.
        assert_eq!(d.step(MicMode::Lower, false, true, at(t0, 3000)), []);
        assert_eq!(d.step(MicMode::Lower, true, true, at(t0, 4000)), []);
        assert_eq!(d.step(MicMode::Lower, false, true, at(t0, 5000)), []);
        assert_eq!(d.step(MicMode::Lower, false, true, at(t0, 7000)), []);
        assert_eq!(
            d.step(MicMode::Lower, false, true, at(t0, 8000)),
            [Act::Restore]
        );
        assert_eq!(d.step(MicMode::Lower, false, true, at(t0, 9000)), []);
    }

    #[test]
    fn pauses_only_playing_music_and_resumes_only_its_own_pause() {
        let (t0, mut d) = (Instant::now(), Ducker::default());
        // Already paused: left alone, and not started afterwards.
        assert_eq!(d.step(MicMode::Pause, true, false, t0), []);
        assert_eq!(d.step(MicMode::Pause, false, false, at(t0, 5000)), []);
        // Playing: paused, then resumed once the microphone is free.
        assert_eq!(
            d.step(MicMode::Pause, true, true, at(t0, 6000)),
            [Act::Pause]
        );
        assert_eq!(d.step(MicMode::Pause, true, false, at(t0, 7000)), []);
        assert_eq!(d.step(MicMode::Pause, false, false, at(t0, 8000)), []);
        assert_eq!(
            d.step(MicMode::Pause, false, false, at(t0, 11000)),
            [Act::Resume]
        );
    }

    #[test]
    fn music_played_again_by_hand_during_the_call_is_not_touched_after_it() {
        let (t0, mut d) = (Instant::now(), Ducker::default());
        assert_eq!(d.step(MicMode::Pause, true, true, t0), [Act::Pause]);
        assert_eq!(d.step(MicMode::Pause, true, true, at(t0, 1000)), []);
        // Paused by hand after the call: stays paused.
        assert_eq!(d.step(MicMode::Pause, false, false, at(t0, 2000)), []);
        assert_eq!(d.step(MicMode::Pause, false, false, at(t0, 9000)), []);
    }

    #[test]
    fn switching_the_setting_off_restores_at_once() {
        let (t0, mut d) = (Instant::now(), Ducker::default());
        assert_eq!(d.step(MicMode::Lower, true, true, t0), [Act::Lower]);
        assert_eq!(
            d.step(MicMode::Off, true, true, at(t0, 500)),
            [Act::Restore]
        );
        assert!(!d.active());
        assert_eq!(d.step(MicMode::Off, true, true, at(t0, 900)), []);
    }

    /// Asks this Mac (read-only). Run with `cargo test -p fxmp live_mic -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_mic_state() {
        eprintln!("microphone in use: {:?}", in_use(&[]));
        #[cfg(target_os = "macos")]
        eprintln!(
            "per process: {:?}, default input: {:?}",
            mac::any_process_recording(),
            mac::default_input_running()
        );
    }

    #[test]
    fn ask_waits_for_an_answer_and_undoes_it_after_the_call() {
        let (t0, mut d) = (Instant::now(), Ducker::default());
        // Nothing playing: nothing to ask.
        assert_eq!(d.step(MicMode::Ask, true, false, t0), []);
        assert_eq!(d.step(MicMode::Ask, true, true, at(t0, 1000)), [Act::Ask]);
        assert_eq!(
            d.step(MicMode::Ask, true, true, at(t0, 2000)),
            [],
            "asked once"
        );
        assert_eq!(d.answer(Answer::Lower, true), Some(Act::Lower));
        assert_eq!(d.answer(Answer::Pause, true), None, "one answer");
        assert_eq!(d.step(MicMode::Ask, false, true, at(t0, 3000)), []);
        assert_eq!(
            d.step(MicMode::Ask, false, true, at(t0, 6000)),
            [Act::Restore]
        );
        // Unanswered: the bar goes once the microphone is free.
        assert_eq!(d.step(MicMode::Ask, true, true, at(t0, 7000)), [Act::Ask]);
        assert_eq!(
            d.step(MicMode::Ask, false, true, at(t0, 8000)),
            [Act::Withdraw]
        );
        assert_eq!(d.answer(Answer::Pause, true), None, "too late");
        // Ignore: nothing happens, and it is not asked again in this call.
        assert_eq!(d.step(MicMode::Ask, true, true, at(t0, 9000)), [Act::Ask]);
        assert_eq!(d.answer(Answer::Ignore, true), None);
        assert_eq!(d.step(MicMode::Ask, true, true, at(t0, 10000)), []);
    }

    #[test]
    fn ask_then_pause_resumes_after_the_call() {
        let (t0, mut d) = (Instant::now(), Ducker::default());
        assert_eq!(d.step(MicMode::Ask, true, true, t0), [Act::Ask]);
        assert_eq!(d.answer(Answer::Pause, true), Some(Act::Pause));
        assert_eq!(d.step(MicMode::Ask, false, false, at(t0, 1000)), []);
        assert_eq!(
            d.step(MicMode::Ask, false, false, at(t0, 4000)),
            [Act::Resume]
        );
    }

    #[test]
    fn only_the_listed_apps_count() {
        let f = app_filter(" Teams, zoom ;");
        assert_eq!(f, ["teams", "zoom"]);
        assert!(app_counts("MicrosoftTeams_8wekyb3d8bbwe", &f));
        assert!(app_counts(
            "C:#Users#u#AppData#Roaming#Zoom#bin#Zoom.exe",
            &f
        ));
        assert!(!app_counts("C:#Program Files#Audacity#Audacity.exe", &f));
        assert!(app_counts("anything", &app_filter("  ")), "empty: any app");
    }

    #[test]
    fn the_level_is_capped() {
        assert_eq!(level(20), 0.2);
        assert_eq!(level(0), 0.0);
        assert_eq!(level(100), 0.8);
    }

    #[test]
    fn the_setting_reads_old_and_new_values() {
        let m: MicMode = serde_json::from_str("\"pause\"").unwrap();
        assert_eq!(m, MicMode::Pause);
        assert_eq!(MicMode::default(), MicMode::Lower);
    }
}
