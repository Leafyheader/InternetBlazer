; The app registers blazer-host with the browsers each time it starts; this takes it out again.
!macro NSIS_HOOK_PREUNINSTALL
  ExecWait '"$INSTDIR\blazer-host.exe" uninstall'
!macroend
