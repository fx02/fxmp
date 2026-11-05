//! "Microphone in use": lowers or pauses the music while some app uses the
//! microphone (a call, a meeting), and undoes it once the microphone is free.
//! Nothing is recorded and no microphone permission is needed: the system is only
//! asked whether its input is in use (macOS: the default input device is running,
//! the signal of the orange dot; Windows: the microphone consent store, the signal
//! of its microphone icon).

use std::time::{Duration, Instant};

use fxmp_core::audio::player::Command;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::commands::{pause_now, play_pause_now};
use crate::state::{lock, AppState};

/// What happens while the microphone is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MicMode {
    Off,
    /// Down to [`DUCK_LEVEL`] of the volume.
    #[default]
    Lower,
    Pause,
}

/// The volume while lowered, as a share of the set one.
pub const DUCK_LEVEL: f32 = 0.2;
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
        if mode != MicMode::Lower && self.lowered {
            self.lowered = false;
            acts.push(Act::Restore);
        }
        if mode != MicMode::Pause {
            self.paused = false;
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
                _ => {}
            }
            return acts;
        }
        self.overridden = false;
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

    /// Something to undo once the microphone is free.
    pub fn active(&self) -> bool {
        self.lowered || self.paused
    }
}

/// Checks the microphone in the background for the app's lifetime.
pub fn start(app: &AppHandle) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("mic-watch".into())
        .spawn(move || {
            let mut ducker = Ducker::default();
            let mut unknown_logged = false;
            loop {
                std::thread::sleep(POLL);
                let st = app.state::<AppState>();
                let mode = st.settings.get().mic_duck;
                if mode == MicMode::Off && !ducker.active() {
                    continue;
                }
                let mic = match in_use() {
                    Some(m) => m,
                    None => {
                        if !unknown_logged {
                            log::info!("microphone use cannot be told here");
                            unknown_logged = true;
                        }
                        false
                    }
                };
                let playing = lock(&st.engine).loaded()
                    && !st.paused.load(std::sync::atomic::Ordering::SeqCst);
                for act in ducker.step(mode, mic, playing, Instant::now()) {
                    log::info!(
                        "microphone {}: {act:?}",
                        if mic { "in use" } else { "free" }
                    );
                    match act {
                        Act::Lower => st.player.send(Command::SetDuck(DUCK_LEVEL)),
                        Act::Restore => st.player.send(Command::SetDuck(1.0)),
                        Act::Pause => pause_now(&st),
                        Act::Resume => play_pause_now(&app, &st, Command::Resume),
                    }
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("cannot watch the microphone: {e}");
    }
}

/// Whether some app is using the microphone (`None`: cannot tell).
#[cfg(target_os = "macos")]
pub fn in_use() -> Option<bool> {
    use objc2_core_audio::{
        kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioHardwarePropertyDefaultInputDevice,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioObjectGetPropertyData,
        AudioObjectPropertyAddress,
    };
    use std::ptr::NonNull;

    fn get_u32(object: u32, selector: u32) -> Option<u32> {
        let address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: valid pointers to a property address, a size and a u32 that
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
        (status == 0).then_some(value)
    }

    let system = kAudioObjectSystemObject as u32;
    let input = get_u32(system, kAudioHardwarePropertyDefaultInputDevice)?;
    if input == 0 {
        return Some(false);
    }
    // A headset that is also the default output runs while music plays: its
    // "running" says nothing about the microphone.
    if get_u32(system, kAudioHardwarePropertyDefaultOutputDevice) == Some(input) {
        return None;
    }
    get_u32(input, kAudioDevicePropertyDeviceIsRunningSomewhere).map(|v| v != 0)
}

/// Whether some app is using the microphone (`None`: cannot tell): an app in the
/// microphone consent store that started using it and has not stopped.
#[cfg(target_os = "windows")]
pub fn in_use() -> Option<bool> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const STORE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

    fn any_active(key: &RegKey) -> bool {
        key.enum_keys().flatten().any(|name| {
            key.open_subkey(&name).is_ok_and(|app| {
                let start: u64 = app.get_value("LastUsedTimeStart").unwrap_or(0);
                let stop: u64 = app.get_value("LastUsedTimeStop").unwrap_or(1);
                start > 0 && stop == 0
            })
        })
    }

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
pub fn in_use() -> Option<bool> {
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
        eprintln!("microphone in use: {:?}", in_use());
    }

    #[test]
    fn the_setting_reads_old_and_new_values() {
        let m: MicMode = serde_json::from_str("\"pause\"").unwrap();
        assert_eq!(m, MicMode::Pause);
        assert_eq!(MicMode::default(), MicMode::Lower);
    }
}
