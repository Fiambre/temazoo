; Temazoo installer (per-user, no admin needed).
; Build: makensis /DVERSION=1.2.3 installer\temazoo.nsi   (run from the repo root)
!include "MUI2.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif

Name "Temazoo"
OutFile "..\dist\Temazoo-Setup-${VERSION}.exe"
Unicode true
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\Temazoo"
InstallDirRegKey HKCU "Software\Temazoo" "InstallDir"
SetCompressor /SOLID lzma

!define MUI_ICON "..\assets\temazoo.ico"
!define MUI_UNICON "..\assets\temazoo.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\temazoo.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Abrir Temazoo"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "Spanish"

Section "Temazoo"
  SetOutPath "$INSTDIR"
  File "..\target\release\temazoo.exe"
  FileOpen $0 "$INSTDIR\installed" w
  FileClose $0
  SetOutPath "$INSTDIR\bin"
  File "..\bin\yt-dlp.exe"

  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Temazoo.lnk" "$INSTDIR\temazoo.exe"
  CreateShortcut "$DESKTOP\Temazoo.lnk" "$INSTDIR\temazoo.exe"

  WriteRegStr HKCU "Software\Temazoo" "InstallDir" "$INSTDIR"
  !define UNKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Temazoo"
  WriteRegStr HKCU "${UNKEY}" "DisplayName" "Temazoo"
  WriteRegStr HKCU "${UNKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNKEY}" "Publisher" "Fiambre"
  WriteRegStr HKCU "${UNKEY}" "DisplayIcon" "$INSTDIR\temazoo.exe"
  WriteRegStr HKCU "${UNKEY}" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""
  WriteRegDWORD HKCU "${UNKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNKEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\temazoo.exe"
  Delete "$INSTDIR\installed"
  Delete "$INSTDIR\bin\yt-dlp.exe"
  RMDir "$INSTDIR\bin"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Temazoo.lnk"
  Delete "$DESKTOP\Temazoo.lnk"
  DeleteRegKey HKCU "${UNKEY}"
  DeleteRegKey HKCU "Software\Temazoo"
  ; Settings and session live in %LOCALAPPDATA%\Temazoo and are kept on purpose.
SectionEnd
