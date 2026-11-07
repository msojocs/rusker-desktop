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
; Install modes (NSIS MultiUser):
;   - Current user (default): %LOCALAPPDATA%\Programs\Rusker Desktop, HKCU.
;   - All users: %ProgramFiles%\Rusker Desktop, HKLM; needs administrator.
; Administrators get a UAC prompt on launch and can pick either mode on the
; "Choose Users" page; standard users install for themselves only. Silent
; installs accept /CurrentUser or /AllUsers. Upgrades default to the mode of
; the existing installation.
;
; The managed WSL2 distro, settings and data are always per user and are never
; removed by the uninstaller.

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

; MultiUser: read HKCU/HKLM INSTALL_KEY to find a previous install's mode and
; directory, and accept /CurrentUser and /AllUsers on the command line.
!define MULTIUSER_EXECUTIONLEVEL Highest
!define MULTIUSER_MUI
!define MULTIUSER_INSTALLMODE_COMMANDLINE
!define MULTIUSER_INSTALLMODE_DEFAULT_CURRENTUSER
!define MULTIUSER_INSTALLMODE_INSTDIR "${APP_NAME}"
!define MULTIUSER_USE_PROGRAMFILES64
!define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_KEY "${INSTALL_KEY}"
!define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_VALUENAME "InstallDir"
!define MULTIUSER_INSTALLMODE_INSTDIR_REGISTRY_KEY "${INSTALL_KEY}"
!define MULTIUSER_INSTALLMODE_INSTDIR_REGISTRY_VALUENAME "InstallDir"
!define MULTIUSER_INIT_TEXT_ADMINREQUIRED "$(MsgAdminRequired)"
!define MULTIUSER_INIT_TEXT_ALLUSERSNOTPOSSIBLE "$(MsgAllUsersNotPossible)"

!include "FileFunc.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "MultiUser.nsh"
!include "MUI2.nsh"

Name "${APP_NAME}"
OutFile "${OUTFILE}"
BrandingText "${APP_NAME} ${VERSION}"

VIProductVersion "${VERSION_NUMERIC}"
VIAddVersionKey /LANG=0 "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=0 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "FileDescription" "${APP_NAME} Installer"
VIAddVersionKey /LANG=0 "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey /LANG=0 "LegalCopyright" "Apache-2.0"

!define MUI_ABORTWARNING
; The installer may be elevated; launch the app through Explorer so it runs
; as the signed-in user rather than as administrator.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApp

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
!insertmacro MULTIUSER_PAGE_INSTALLMODE
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

LangString MsgAppRunning ${LANG_SIMPCHINESE} "${APP_NAME} 正在运行。请先从托盘菜单退出程序（所有用户），然后点击“重试”。"
LangString MsgAppRunning ${LANG_ENGLISH} "${APP_NAME} is running. Quit it from the tray menu (for every user), then click Retry."
LangString MsgRequire64 ${LANG_SIMPCHINESE} "${APP_NAME} 仅支持 64 位 Windows。"
LangString MsgRequire64 ${LANG_ENGLISH} "${APP_NAME} requires 64-bit Windows."
LangString MsgAdminRequired ${LANG_SIMPCHINESE} "${APP_NAME} 需要管理员权限。"
LangString MsgAdminRequired ${LANG_ENGLISH} "${APP_NAME} requires administrator privileges."
LangString MsgAllUsersNotPossible ${LANG_SIMPCHINESE} "当前账户没有管理员权限，无法为所有用户安装 ${APP_NAME}。"
LangString MsgAllUsersNotPossible ${LANG_ENGLISH} "Your account does not have the privileges to install ${APP_NAME} for all users."
LangString MsgUninstallNeedsAdmin ${LANG_SIMPCHINESE} "${APP_NAME} 是为所有用户安装的，卸载需要管理员权限。"
LangString MsgUninstallNeedsAdmin ${LANG_ENGLISH} "${APP_NAME} was installed for all users; uninstalling it requires administrator privileges."

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
  ; Before MULTIUSER_INIT: it reads the previous install from HKLM.
  SetRegView 64
  !insertmacro MULTIUSER_INIT
FunctionEnd

Function LaunchApp
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
FunctionEnd

Function un.onInit
  SetRegView 64
  ; The uninstaller knows where it lives; pick the mode that owns this
  ; directory instead of guessing, since both modes may be installed.
  StrCpy $R9 $INSTDIR
  !insertmacro MULTIUSER_UNINIT
  ReadRegStr $R8 HKLM "${INSTALL_KEY}" "InstallDir"
  ${If} $R8 == $R9
    ${If} $MultiUser.Privileges != "Admin"
    ${AndIf} $MultiUser.Privileges != "Power"
      MessageBox MB_OK|MB_ICONSTOP "$(MsgUninstallNeedsAdmin)" /SD IDOK
      Quit
    ${EndIf}
    Call un.MultiUser.InstallMode.AllUsers
  ${Else}
    Call un.MultiUser.InstallMode.CurrentUser
  ${EndIf}
  StrCpy $INSTDIR $R9
FunctionEnd

Section "Install"
  !insertmacro WaitForAppExit

  SetOutPath "$INSTDIR"
  File "/oname=${APP_EXE}" "${SOURCE_EXE}"
  File "/oname=LICENSE.txt" "..\..\LICENSE"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; SetShellVarContext (set by MultiUser) points these at the current user's
  ; or the shared Start menu/Desktop; SHCTX is HKCU or HKLM accordingly.
  CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"

  WriteRegStr SHCTX "${INSTALL_KEY}" "InstallDir" "$INSTDIR"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "URLInfoAbout" "${APP_URL}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe" /$MultiUser.InstallMode'
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /$MultiUser.InstallMode /S'
  WriteRegDWORD SHCTX "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD SHCTX "${UNINSTALL_KEY}" "NoRepair" 1

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD SHCTX "${UNINSTALL_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  !insertmacro WaitForAppExit

  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey SHCTX "${UNINSTALL_KEY}"
  DeleteRegKey SHCTX "${INSTALL_KEY}"
  DeleteRegKey /ifempty SHCTX "Software\Rusker"
SectionEnd
