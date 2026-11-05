; fxmp installer hooks (bundle.windows.nsis.installerHooks in tauri.conf.json).

; After installing: offer a portable installation, i.e. a fxmp-data folder beside
; fxmp.exe, where the app then keeps its settings, library and log (see
; src-tauri/src/portable.rs). Not asked in a silent install or an update of a
; copy that already has the folder. The uninstaller never removes the folder.
!macro NSIS_HOOK_POSTINSTALL
  IfSilent fxmp_portable_done
  IfFileExists "$INSTDIR\fxmp-data\*.*" fxmp_portable_done
  MessageBox MB_YESNO|MB_ICONQUESTION "Portable installation?$\r$\n$\r$\nYes: fxmp keeps its settings, library and log in$\r$\n$INSTDIR\fxmp-data$\r$\n(for a USB stick or a folder you move around; passwords stay on each computer).$\r$\n$\r$\nNo: in your Windows user profile (the usual choice)." /SD IDNO IDNO fxmp_portable_done
  CreateDirectory "$INSTDIR\fxmp-data"
  fxmp_portable_done:
!macroend
