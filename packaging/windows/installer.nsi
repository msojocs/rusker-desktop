; Rusker Desktop NSIS installer.
;
; Build from the repository root, after `cargo build --release`. makensis
; resolves relative paths from this script's directory, so pass absolute
; SOURCE_EXE/OUTFILE paths:
;   makensis /DVERSION=0.1.0 /DVERSION_NUMERIC=0.1.0.0 ^
;            /DSOURCE_EXE=%CD%\target\release\rusker-desktop.exe ^
;            /DOUTFILE=%CD%\dist\rusker-desktop-0.1.0-windows-x64-setup.exe ^
;            packaging\windows\installer.nsi
;
; Installs per user (no UAC prompt): the managed WSL2 distro, settings and
; data are all per user as well. User data is never removed by the uninstaller.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma

!ifndef VERSION
  !error "VERSION must be defined, e.g. /DVERSION=0.1.0"
!endif
!ifndef VERSION_NUMERIC
  !error "VERSION_NUMERIC must be defined as X.X.X.X, e.g. /DVERSION_NUMERIC=0.1.0.0"
!endif
!ifndef SOURCE_EXE
  !define SOURCE_EXE "..\..\target\release\rusker-desktop.exe"
!endif
!ifndef OUTFILE
  !define OUTFILE "rusker-desktop-${VERSION}-windows-x64-setup.exe"
!endif

!define APP_NAME "Rusker Desktop"
!ifndef APP_EXE
  !define APP_EXE "rusker-desktop.exe"
!endif
!define APP_PUBLISHER "Rusker"
!define APP_URL "https://github.com/msojocs/rusker-desktop"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\RuskerDesktop"
!define INSTALL_KEY "Software\Rusker\Rusker Desktop"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"

Name "${APP_NAME}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\${APP_NAME}"
InstallDirRegKey HKCU "${INSTALL_KEY}" "InstallDir"
BrandingText "${APP_NAME} ${VERSION}"

VIProductVersion "${VERSION_NUMERIC}"
VIAddVersionKey /LANG=0 "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=0 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "FileDescription" "${APP_NAME} Installer"
VIAddVersionKey /LANG=0 "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey /LANG=0 "LegalCopyright" "Apache-2.0"

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

LangString MsgAppRunning ${LANG_SIMPCHINESE} "${APP_NAME} 正在运行。请先从托盘菜单退出程序，然后点击“重试”。"
LangString MsgAppRunning ${LANG_ENGLISH} "${APP_NAME} is running. Quit it from the tray menu, then click Retry."
LangString MsgRequire64 ${LANG_SIMPCHINESE} "${APP_NAME} 仅支持 64 位 Windows。"
LangString MsgRequire64 ${LANG_ENGLISH} "${APP_NAME} requires 64-bit Windows."

; Asks the user to quit a running instance; files cannot be replaced while the
; exe is locked. The app is not killed forcibly so it can shut down its engine.
!macro WaitForAppExit
  ${Do}
    nsExec::ExecToStack 'cmd.exe /C tasklist /FI "IMAGENAME eq ${APP_EXE}" /NH | find /I "${APP_EXE}"'
    Pop $0
    Pop $1
    ${If} $0 != 0
      ${Break}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "$(MsgAppRunning)" /SD IDCANCEL IDRETRY +2
    Abort
  ${Loop}
!macroend

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "$(MsgRequire64)"
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

Section "Install"
  !insertmacro WaitForAppExit

  SetOutPath "$INSTDIR"
  File "/oname=${APP_EXE}" "${SOURCE_EXE}"
  File "/oname=LICENSE.txt" "..\..\LICENSE"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"

  WriteRegStr HKCU "${INSTALL_KEY}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "${APP_URL}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  !insertmacro WaitForAppExit

  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  DeleteRegKey HKCU "${INSTALL_KEY}"
  DeleteRegKey /ifempty HKCU "Software\Rusker"
SectionEnd
