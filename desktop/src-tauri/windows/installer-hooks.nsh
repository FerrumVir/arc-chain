!macro NSIS_HOOK_PREUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 \
      "Deleting the application data also deletes the ARC wallet keys stored on this computer. You will need your 12-word recovery phrase to get the wallet back.$\r$\n$\r$\nDelete anyway?" \
      /SD IDNO IDYES arc_delete_app_data_confirmed
    StrCpy $DeleteAppDataCheckboxState 0
    arc_delete_app_data_confirmed:
  ${EndIf}
!macroend
