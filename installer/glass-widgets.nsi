; Glass Widgets installer (NSIS, per user, no administrator rights).
;   makensis /DVERSION=0.2.0 /DEXEPATH=..\target\release\mswidgets.exe /DOUTFILE=..\dist\GlassWidgets_0.2.0_x64-setup.exe installer\glass-widgets.nsi
; tools\release.ps1 passes these three defines. The artwork (icon, sidebar, header) comes from tools\installer_art.py.
; English and Russian: NSIS picks the language of Windows on its own.
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
!define SITE "https://github.com/chvdbhcx-del/glass-widgets"
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
Icon "icon.ico"
UninstallIcon "icon.ico"
!define MUI_ICON "icon.ico"
!define MUI_UNICON "icon.ico"
!define MUI_ABORTWARNING
!define MUI_WELCOMEFINISHPAGE_BITMAP "sidebar.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "sidebar.bmp"
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_BITMAP "header.bmp"
!define MUI_HEADERIMAGE_UNBITMAP "header.bmp"
!define MUI_HEADERIMAGE_RIGHT

!define MUI_WELCOMEPAGE_TITLE "$(WELCOME_TITLE)"
!define MUI_WELCOMEPAGE_TEXT "$(WELCOME_TEXT)"
!define MUI_FINISHPAGE_TITLE "${NAME}"
!define MUI_FINISHPAGE_TEXT "$(FINISH_TEXT)"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "$(FINISH_RUN)"
!define MUI_FINISHPAGE_LINK "$(FINISH_LINK)"
!define MUI_FINISHPAGE_LINK_LOCATION "${SITE}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Russian"

LangString WELCOME_TITLE ${LANG_ENGLISH} "Welcome to ${NAME}"
LangString WELCOME_TITLE ${LANG_RUSSIAN} "Добро пожаловать в ${NAME}"
LangString WELCOME_TEXT ${LANG_ENGLISH} "Native Direct2D desktop widgets with a Liquid-Glass look: clock, calendar, weather, music, system monitor and more.$\r$\n$\r$\nThis wizard installs ${NAME} ${VERSION} for your account. No administrator rights needed."
LangString WELCOME_TEXT ${LANG_RUSSIAN} "Нативные виджеты рабочего стола на Direct2D в стиле Liquid Glass: часы, календарь, погода, музыка, мониторинг системы и другие.$\r$\n$\r$\nМастер установит ${NAME} ${VERSION} для вашей учётной записи. Права администратора не нужны."
LangString FINISH_RUN ${LANG_ENGLISH} "Start ${NAME}"
LangString FINISH_RUN ${LANG_RUSSIAN} "Запустить ${NAME}"
LangString FINISH_LINK ${LANG_ENGLISH} "Project page on GitHub"
LangString FINISH_LINK ${LANG_RUSSIAN} "Страница проекта на GitHub"
LangString FINISH_TEXT ${LANG_ENGLISH} "${NAME} is installed. Its icon sits in the notification area: right-click it for settings, or press Ctrl+Alt+G."
LangString FINISH_TEXT ${LANG_RUSSIAN} "${NAME} установлен. Значок находится в области уведомлений: нажмите на нём правой кнопкой для настроек или нажмите Ctrl+Alt+G."
LangString SEC_MAIN ${LANG_ENGLISH} "${NAME}"
LangString SEC_MAIN ${LANG_RUSSIAN} "${NAME}"
LangString SEC_DESK ${LANG_ENGLISH} "Desktop shortcut"
LangString SEC_DESK ${LANG_RUSSIAN} "Ярлык на столе"
LangString SEC_AUTO ${LANG_ENGLISH} "Start with Windows"
LangString SEC_AUTO ${LANG_RUSSIAN} "Автозапуск"
LangString DESC_MAIN ${LANG_ENGLISH} "The widgets, the tray icon and the settings panel."
LangString DESC_MAIN ${LANG_RUSSIAN} "Виджеты, значок в трее и панель настроек."
LangString DESC_DESK ${LANG_ENGLISH} "Adds a shortcut to the desktop."
LangString DESC_DESK ${LANG_RUSSIAN} "Добавляет ярлык на рабочий стол."
LangString DESC_AUTO ${LANG_ENGLISH} "Starts the widgets when you sign in. You can change this later in the settings."
LangString DESC_AUTO ${LANG_RUSSIAN} "Запускает виджеты при входе в систему. Позже это можно изменить в настройках."

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "FileDescription" "${NAME} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT License"

Section "$(SEC_MAIN)" SecMain
  SectionIn RO
  ; an upgrade replaces the exe: close a running copy first (it is this product's own process)
  nsExec::Exec 'taskkill /F /IM ${EXE}'
  Sleep 500
  SetOutPath "$INSTDIR"
  File "${EXEPATH}"
  File "..\LICENSE"
  File "..\README.md"
  File "..\assets\fonts\OFL.txt"
  File "..\assets\fonts\Inter-LICENSE.txt"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateDirectory "$SMPROGRAMS\${NAME}"
  CreateShortcut "$SMPROGRAMS\${NAME}\${NAME}.lnk" "$INSTDIR\${EXE}"
  CreateShortcut "$SMPROGRAMS\${NAME}\Uninstall ${NAME}.lnk" "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "Software\GlassWidgets" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNKEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNKEY}" "Publisher" "chvdbhcx-del"
  WriteRegStr HKCU "${UNKEY}" "URLInfoAbout" "${SITE}"
  WriteRegStr HKCU "${UNKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNKEY}" "DisplayIcon" "$INSTDIR\${EXE}"
  WriteRegStr HKCU "${UNKEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "${UNKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNKEY}" "NoRepair" 1
SectionEnd

; the optional ones are unticked: a desktop icon and autostart are the user's choice
Section /o "$(SEC_DESK)" SecDesk
  CreateShortcut "$DESKTOP\${NAME}.lnk" "$INSTDIR\${EXE}"
SectionEnd

Section /o "$(SEC_AUTO)" SecAuto
  WriteRegStr HKCU "${RUNKEY}" "GlassWidgets" '"$INSTDIR\${EXE}"'
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "$(DESC_MAIN)"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesk} "$(DESC_DESK)"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAuto} "$(DESC_AUTO)"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  nsExec::Exec 'taskkill /F /IM ${EXE}'
  Sleep 500
  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\OFL.txt"
  Delete "$INSTDIR\Inter-LICENSE.txt"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${NAME}\${NAME}.lnk"
  Delete "$SMPROGRAMS\${NAME}\Uninstall ${NAME}.lnk"
  RMDir "$SMPROGRAMS\${NAME}"
  Delete "$DESKTOP\${NAME}.lnk"
  DeleteRegValue HKCU "${RUNKEY}" "GlassWidgets"
  DeleteRegKey HKCU "${UNKEY}"
  DeleteRegKey HKCU "Software\GlassWidgets"
  ; settings (%APPDATA%\macshell\config.json) are left in place on purpose
SectionEnd
