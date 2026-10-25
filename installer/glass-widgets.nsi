; Glass Widgets installer (NSIS, per user, no administrator rights).
;   makensis /DVERSION=0.1.0 /DEXEPATH=..\target\release\mswidgets.exe /DOUTFILE=..\dist\GlassWidgets_0.1.0_x64-setup.exe installer\glass-widgets.nsi
; tools\release.ps1 passes these three defines.
Unicode true
!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef EXEPATH
  !define EXEPATH "..\target\release\mswidgets.exe"
!endif
!ifndef OUTFILE
  !define OUTFILE "..\dist\GlassWidgets_${VERSION}_x64-setup.exe"
!endif

!define NAME "Glass Widgets"
!define EXE "mswidgets.exe"
!define UNKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\GlassWidgets"
!define RUNKEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "${NAME}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Glass Widgets"
InstallDirRegKey HKCU "Software\GlassWidgets" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
BrandingText "${NAME} ${VERSION}"

!include MUI2.nsh
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${NAME}"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\LICENSE"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Russian"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "FileDescription" "${NAME} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT License"

Section "${NAME}" SecMain
  SectionIn RO
  ; an upgrade replaces the exe: close a running copy first (it is this product's own process)
  nsExec::Exec 'taskkill /F /IM ${EXE}'
  Sleep 500
  SetOutPath "$INSTDIR"
  File "${EXEPATH}"
  File "..\LICENSE"
  File "..\README.md"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateDirectory "$SMPROGRAMS\${NAME}"
  CreateShortcut "$SMPROGRAMS\${NAME}\${NAME}.lnk" "$INSTDIR\${EXE}"
  CreateShortcut "$SMPROGRAMS\${NAME}\Uninstall ${NAME}.lnk" "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "Software\GlassWidgets" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNKEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNKEY}" "Publisher" "chvdbhcx-del"
  WriteRegStr HKCU "${UNKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNKEY}" "DisplayIcon" "$INSTDIR\${EXE}"
  WriteRegStr HKCU "${UNKEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "${UNKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNKEY}" "NoRepair" 1
SectionEnd

; unticked by default: autostart is the user's choice
Section /o "Start with Windows" SecAuto
  WriteRegStr HKCU "${RUNKEY}" "GlassWidgets" '"$INSTDIR\${EXE}"'
SectionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /F /IM ${EXE}'
  Sleep 500
  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${NAME}\${NAME}.lnk"
  Delete "$SMPROGRAMS\${NAME}\Uninstall ${NAME}.lnk"
  RMDir "$SMPROGRAMS\${NAME}"
  DeleteRegValue HKCU "${RUNKEY}" "GlassWidgets"
  DeleteRegKey HKCU "${UNKEY}"
  DeleteRegKey HKCU "Software\GlassWidgets"
  ; settings (%APPDATA%\macshell\config.json) are left in place on purpose
SectionEnd
