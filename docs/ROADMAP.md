# fxmp roadmap

What the owner asked for that is not done yet. The order is not a priority.

## Later

- **Where the data lives.** Document where the settings, the database and the
  logs are on macOS and Windows. Done for the paths after the rename (README,
  "Where the app keeps its data"); still open: a log file, since today the log
  only goes to the standard error output (not visible in a Windows release
  build).
- **Portable mode.**
  - The catalog database can be copied between machines that reach the same
    shares, since its paths are relative per library.
  - An installable/portable variant that keeps its data next to the executable.
  - Keychain credentials are not portable: on another machine (or in the
    portable variant) the passwords are entered again.
- **Lower memory use.** Consider a native UI instead of the WebView.
- **A web version** hosted on Debian.
- **An Android app.**
- **F8/F9 media keys on Windows.** souvlaki is disabled there (it needs a
  window handle), so the media keys and the F8/F9 shortcuts work only on macOS.
- **Lower the music when the microphone is in use** (calls, meetings). Detect
  that some app is using the microphone, without recording or needing microphone
  permission:
  - macOS: CoreAudio `kAudioDevicePropertyDeviceIsRunningSomewhere` on the
    default input device (the same signal as the orange dot). Which app is
    using it is not easily known.
  - Windows: the active capture sessions (`IAudioSessionManager2` on the
    capture endpoint) or the microphone consent store, which also tell which
    app it is (e.g. lower only for Teams/Zoom).
  - Setting: Off / Offer (a small notification "Microphone in use — lower the
    music?" with Lower / Pause) / Lower automatically (fade to a chosen level,
    e.g. 20%) / Pause automatically.
  - When the microphone is free again, return to the previous volume, or resume
    after an automatic pause, after a 2–3 s delay so short mute toggles don't
    pump the volume. Music that was already paused before the call is never
    started.
