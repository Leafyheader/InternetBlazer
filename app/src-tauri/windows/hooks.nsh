; The app registers blazer-host with the browsers each time it starts; this takes it out again.
!macro NSIS_HOOK_PREUNINSTALL
  ExecWait '"$INSTDIR\blazer-host.exe" uninstall'
!macroend

; The full installer ships yt-dlp, deno and ffmpeg in tools\, and updates install without them,
; so the uninstaller of an updated copy doesn't list those files. Removing the app removes them;
; an update, which runs this uninstaller only to replace the app, keeps them.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    RMDir /r "$INSTDIR\tools"
  ${EndIf}
!macroend
