; Puts `ladex` on PATH and lets the app and the CLI through Windows Firewall, so no "Allow access?" prompt appears.

!define LADEX_APP_RULE "LADEX"
!define LADEX_CLI_RULE "LADEX command line"
!define LADEX_PATH_SCRIPT 'powershell -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\cli-path.ps1"'

!macro LADEX_REMOVE_FIREWALL_RULES
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="${LADEX_APP_RULE}"'
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="${LADEX_CLI_RULE}"'
!macroend

; Tells open terminals and Explorer that PATH changed (HWND_BROADCAST, WM_SETTINGCHANGE).
!macro LADEX_ANNOUNCE_PATH_CHANGE
  SendMessage 0xFFFF 0x001A 0 "STR:Environment" /TIMEOUT=5000
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Removed first so reinstalling never leaves duplicate rules behind.
  !insertmacro LADEX_REMOVE_FIREWALL_RULES
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="${LADEX_APP_RULE}" dir=in action=allow program="$INSTDIR\${MAINBINARYNAME}.exe" enable=yes profile=any'
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="${LADEX_CLI_RULE}" dir=in action=allow program="$INSTDIR\ladex.exe" enable=yes profile=any'
  nsExec::ExecToLog '${LADEX_PATH_SCRIPT} -Action Add -Folder "$INSTDIR"'
  !insertmacro LADEX_ANNOUNCE_PATH_CHANGE
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro LADEX_REMOVE_FIREWALL_RULES
  nsExec::ExecToLog '${LADEX_PATH_SCRIPT} -Action Remove -Folder "$INSTDIR"'
  !insertmacro LADEX_ANNOUNCE_PATH_CHANGE
!macroend
