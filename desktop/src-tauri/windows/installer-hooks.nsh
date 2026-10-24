; ARC desktop: hooks for the Tauri-generated Windows (NSIS) installer.
;
; Wired through `bundle.windows.nsis.installerHooks` in tauri.conf.json.
; Tauri's installer.nsi template `!include`s this file and inserts each macro
; it finds at a fixed point; the names must match the template's
; `!ifmacrodef` checks exactly:
;
;   NSIS_HOOK_PREINSTALL      Section Install, before the running app is closed
;   NSIS_HOOK_POSTINSTALL     Section Install, after files and shortcuts
;   NSIS_HOOK_PREUNINSTALL    Section Uninstall, before anything is removed
;   NSIS_HOOK_POSTUNINSTALL   Section Uninstall, after app data was removed
;
; The uninstaller's confirm page carries Tauri's "Delete the application data"
; checkbox. For ARC that data (%APPDATA%\network.arc.desktop) holds the wallet
; key, the recovery phrase and the node identity. The stock uninstaller deletes
; it without a second word, and a manual upgrade reaches that uninstaller
; through the installer's "Uninstall before installing" choice, so a user
; upgrading ARC could delete their wallet keys while meaning to tidy up.
;
; $DeleteAppDataCheckboxState and $UpdateMode are global `Var`s of the
; template, declared before `Section Uninstall` inserts this hook, and read
; again by the template's own deletion step after it. Silent (/S) and passive
; (/P) runs never show the confirm page, so the checkbox state stays unset and
; nothing is deleted; `/SD IDNO` keeps the same default should a silent run
; ever reach the question.

!macro NSIS_HOOK_PREUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 \
      "Deleting the application data also deletes the ARC wallet keys stored on this computer. You will need your 12-word recovery phrase to get the wallet back.$\r$\n$\r$\nDelete anyway?" \
      /SD IDNO IDYES arc_delete_app_data_confirmed
    ; No, or the silent default: keep the application data.
    StrCpy $DeleteAppDataCheckboxState 0
    arc_delete_app_data_confirmed:
  ${EndIf}
!macroend
