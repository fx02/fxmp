# fxmp roadmap

What the owner asked for that is not done yet. The order is not a priority.

## Later

- **Lower memory use.** Consider a native UI instead of the WebView.
- **A web version** hosted on Debian.
- **Android:** local folders on the phone; a release build in CI.

## Idea: visualization (MilkDrop style)

Below the player, where the views are, a visualization; a double click (a
double tap on Android) takes it to the full screen. Butterchurn (MilkDrop in
WebGL, MIT licence) with its presets, bundled with the app (the page loads
nothing from outside). The music plays in Rust, not in the WebView, so the
player sends the sound it plays (a few thousand samples, about 30 times a
second, over a Tauri channel) and the page feeds them to Butterchurn; a
simpler built-in visualization (spectrum bars) could come first. Costs some
CPU and GPU while shown, nothing when hidden.

## Planned: Linux (Fedora, Arch, Debian, RHEL)

Tauri builds for Linux as is; the work is packaging and the desktop parts.
- **Packages:** `.deb` (Debian, Ubuntu), `.rpm` (Fedora, RHEL), an AppImage
  (any distribution, also Arch), and a `PKGBUILD` for the AUR. CI builds them
  on an Ubuntu runner.
- **Needs on the system:** WebKitGTK 4.1, a Secret Service keyring (GNOME
  Keyring or KWallet) for the passwords, ALSA/PipeWire for sound,
  libayatana-appindicator for the tray icon. RHEL may lack a recent WebKitGTK:
  there the AppImage is the way.
- **Code:** `keyring` with its Linux backend; media keys through MPRIS
  (souvlaki already does it); "lower or pause for calls" through PipeWire
  (later, off until then); start at login through an XDG autostart file
  (tauri-plugin-autostart).
- **Checks:** start, tray, media keys, keyring and playback on Fedora, Debian
  and Arch (virtual machines).

## Planned: iOS (iPhone, iPad)

Tauri builds for iOS; the Android work (`src/mobile`) is the model.
- **Needs:** a Mac with Xcode and an Apple Developer Program membership
  (99 USD a year) to put the app on a phone for longer than 7 days.
- **Code:** background playback (the `audio` background mode and an
  AVAudioSession in a small Swift plugin), Now Playing and the lock-screen
  controls (MPNowPlayingInfoCenter, MPRemoteCommandCenter), the Keychain for
  the passwords, and the local network permission text for SMB.
- **Distribution:** TestFlight first (up to 10,000 testers, a build lasts 90
  days, light review), then the App Store (full review: the reviewer needs a
  way to hear music, e.g. a demo share).

## Done
- **Android app** (2026-11-21): test APK for Android 8+ (arm64, armv7), playback with the screen off, passwords in the Android Keystore.
- **Unsupported genres** (2026-11-21): tag names that are no known genre get their own section instead of Unknown.
- **App icon** (2026-11-17): smaller, regular-weight "fxmp" text and a larger, centred play triangle (`src-tauri/icons/gen_app_icon.py`).
