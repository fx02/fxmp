# fxmp roadmap

What the owner asked for that is not done yet. The order is not a priority.

## Later

- **Tray icon on the dark Windows taskbar.** The tray icon is the macOS template
  glyph (black on transparent), so it is invisible on the dark taskbar theme.
  Add a grey/white variant, or follow the system theme.
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
