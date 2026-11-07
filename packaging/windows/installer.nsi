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
; Install modes:
;   - Current user (default): %LOCALAPPDATA%\Programs\Rusker Desktop, HKCU.
;   - All users: %ProgramFiles%\Rusker Desktop, HKLM; needs administrator.
; The installer starts without elevation. Choosing "All users" and clicking
; Next relaunches it through UAC with /AllUsers /RELAUNCHED; the elevated
; copy skips straight to the directory page and the original exits. Silent
; installs accept /CurrentUser or /AllUsers (the latter prompts for UAC).
; Upgrades default to the mode of the existing installation. The uninstaller
; likewise elevates only when removing an all-users installation.
;
; The managed WSL2 distro, settings and data are always per user and are never
; removed by the uninstaller.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel user

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
!define APP_ICON "rusker.ico"

!include "FileFunc.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "nsDialogs.nsh"
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

; "CurrentUser" or "AllUsers"; also passed to the uninstaller.
Var InstallMode
; 1 when this process is the elevated copy started by the mode page.
Var Relaunched
; INSTDIR given with /D=, which always wins over the per-mode default.
Var CmdLineDir
; Mode the current INSTDIR default was computed for, so going Back and Next
; again keeps a directory the user edited.
Var DirMode
Var ModeRadioCurrent
Var ModeRadioAll

!define MUI_ABORTWARNING
!define MUI_ICON "..\..\assets\icons\${APP_ICON}"
!define MUI_UNICON "..\..\assets\icons\${APP_ICON}"
; The installer may be elevated; launch the app through Explorer so it runs
; as the signed-in user rather than as administrator.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApp

!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipWhenRelaunched
!insertmacro MUI_PAGE_WELCOME
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipWhenRelaunched
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
Page custom ModePage ModePageLeave
!define MUI_PAGE_CUSTOMFUNCTION_SHOW DirectoryPageShow
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
LangString MsgElevationFailed ${LANG_SIMPCHINESE} "为所有用户安装需要管理员权限。未获得授权，请重试或改为仅为我安装。"
LangString MsgElevationFailed ${LANG_ENGLISH} "Installing for all users requires administrator privileges, which were not granted. Try again or install for yourself only."
LangString MsgStillNotAdmin ${LANG_SIMPCHINESE} "未能以管理员身份运行，无法为所有用户安装 ${APP_NAME}。"
LangString MsgStillNotAdmin ${LANG_ENGLISH} "Not running as administrator; ${APP_NAME} cannot be installed for all users."
LangString MsgUninstallNeedsAdmin ${LANG_SIMPCHINESE} "${APP_NAME} 是为所有用户安装的，卸载需要管理员权限。"
LangString MsgUninstallNeedsAdmin ${LANG_ENGLISH} "${APP_NAME} was installed for all users; uninstalling it requires administrator privileges."
LangString ModePageTitle ${LANG_SIMPCHINESE} "选择安装方式"
LangString ModePageTitle ${LANG_ENGLISH} "Choose Installation Options"
LangString ModePageSubtitle ${LANG_SIMPCHINESE} "选择为哪些用户安装 ${APP_NAME}。"
LangString ModePageSubtitle ${LANG_ENGLISH} "Choose who ${APP_NAME} is installed for."
LangString ModeCurrentUser ${LANG_SIMPCHINESE} "仅为我安装(&M)"
LangString ModeCurrentUser ${LANG_ENGLISH} "Only for &me"
LangString ModeCurrentUserHint ${LANG_SIMPCHINESE} "安装到你的用户目录，不需要管理员权限。"
LangString ModeCurrentUserHint ${LANG_ENGLISH} "Installs into your user profile; no administrator privileges needed."
LangString ModeAllUsers ${LANG_SIMPCHINESE} "为所有用户安装(&A)"
LangString ModeAllUsers ${LANG_ENGLISH} "For &anyone who uses this computer"
LangString ModeAllUsersHint ${LANG_SIMPCHINESE} "安装到 Program Files，点击“下一步”时需要管理员授权。"
LangString ModeAllUsersHint ${LANG_ENGLISH} "Installs into Program Files; clicking Next asks for administrator approval."

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

; Install-mode helpers, defined for both the installer and the uninstaller.
; SetShellVarContext also selects what SHCTX, $SMPROGRAMS and $DESKTOP mean.
!macro MODE_FUNCTIONS UN
  Function ${UN}UseAllUsers
    StrCpy $InstallMode "AllUsers"
    SetShellVarContext all
  FunctionEnd

  Function ${UN}UseCurrentUser
    StrCpy $InstallMode "CurrentUser"
    SetShellVarContext current
  FunctionEnd

  ; Pushes "1" when the process token is an elevated administrator. Under
  ; UAC an unelevated administrator reports "User".
  Function ${UN}IsAdmin
    UserInfo::GetAccountType
    Pop $0
    ${If} $0 == "Admin"
      Push 1
    ${Else}
      Push 0
    ${EndIf}
  FunctionEnd
!macroend
!insertmacro MODE_FUNCTIONS ""
!insertmacro MODE_FUNCTIONS "un."

; Applies $R0 ("AllUsers"/"CurrentUser") and, unless /D= was given or the user
; already edited the directory for this mode, the default INSTDIR: the
; existing install of that mode, else the standard location.
Function ApplyMode
  ${If} $R0 == "AllUsers"
    Call UseAllUsers
  ${Else}
    Call UseCurrentUser
  ${EndIf}
  ${If} $CmdLineDir != ""
  ${OrIf} $DirMode == $InstallMode
    Return
  ${EndIf}
  StrCpy $DirMode $InstallMode
  ReadRegStr $INSTDIR SHCTX "${INSTALL_KEY}" "InstallDir"
  ${If} $INSTDIR != ""
    Return
  ${EndIf}
  ${If} $InstallMode == "AllUsers"
    StrCpy $INSTDIR "$PROGRAMFILES64\${APP_NAME}"
  ${Else}
    StrCpy $INSTDIR "$LOCALAPPDATA\Programs\${APP_NAME}"
  ${EndIf}
FunctionEnd

; Starts this installer again through UAC for an all-users install. Sets the
; error flag when it could not be started (e.g. the user declined UAC).
Function RelaunchElevated
  StrCpy $1 ""
  ${If} ${Silent}
    StrCpy $1 "/S "
  ${EndIf}
  ClearErrors
  ExecShell "runas" "$EXEPATH" '$1/AllUsers /RELAUNCHED /LANGID=$LANGUAGE'
FunctionEnd

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "$(MsgRequire64)"
    Abort
  ${EndIf}
  SetRegView 64
  StrCpy $CmdLineDir $INSTDIR

  ${GetParameters} $R1
  ClearErrors
  ${GetOptions} $R1 "/RELAUNCHED" $R2
  ${IfNot} ${Errors}
    StrCpy $Relaunched 1
  ${EndIf}
  ClearErrors
  ${GetOptions} $R1 "/LANGID=" $R2
  ${IfNot} ${Errors}
  ${AndIf} $R2 != ""
    StrCpy $LANGUAGE $R2
  ${EndIf}

  ; Mode: command line, else the existing install (per user first), else
  ; per user.
  ClearErrors
  ${GetOptions} $R1 "/AllUsers" $R2
  ${IfNot} ${Errors}
    StrCpy $R0 "AllUsers"
  ${Else}
    ClearErrors
    ${GetOptions} $R1 "/CurrentUser" $R2
    ${IfNot} ${Errors}
      StrCpy $R0 "CurrentUser"
    ${Else}
      StrCpy $R0 "CurrentUser"
      ReadRegStr $R2 HKCU "${INSTALL_KEY}" "InstallDir"
      ${If} $R2 == ""
        ReadRegStr $R2 HKLM "${INSTALL_KEY}" "InstallDir"
        ${If} $R2 != ""
          StrCpy $R0 "AllUsers"
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}

  ${If} $R0 == "AllUsers"
    Call IsAdmin
    Pop $R2
    ${If} $R2 != 1
      ${If} $Relaunched == 1
        MessageBox MB_OK|MB_ICONSTOP "$(MsgStillNotAdmin)" /SD IDOK
        SetErrorLevel 740
        Quit
      ${EndIf}
      ; Without UI there is no mode page; elevate right away.
      ${If} ${Silent}
        Call RelaunchElevated
        ${If} ${Errors}
          SetErrorLevel 740
        ${EndIf}
        Quit
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Call ApplyMode
FunctionEnd

Function SkipWhenRelaunched
  ${If} $Relaunched == 1
    Abort
  ${EndIf}
FunctionEnd

Function ModePage
  ${If} $Relaunched == 1
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "$(ModePageTitle)" "$(ModePageSubtitle)"
  nsDialogs::Create 1018
  Pop $0
  ${If} $0 == error
    Abort
  ${EndIf}

  ${NSD_CreateRadioButton} 0 10u 100% 12u "$(ModeCurrentUser)"
  Pop $ModeRadioCurrent
  ${NSD_OnClick} $ModeRadioCurrent ModePageUpdateShield
  ${NSD_CreateLabel} 12u 24u -12u 20u "$(ModeCurrentUserHint)"
  Pop $0

  ${NSD_CreateRadioButton} 0 52u 100% 12u "$(ModeAllUsers)"
  Pop $ModeRadioAll
  ${NSD_OnClick} $ModeRadioAll ModePageUpdateShield
  ${NSD_CreateLabel} 12u 66u -12u 20u "$(ModeAllUsersHint)"
  Pop $0

  ${If} $InstallMode == "AllUsers"
    ${NSD_Check} $ModeRadioAll
  ${Else}
    ${NSD_Check} $ModeRadioCurrent
  ${EndIf}
  Call ModePageUpdateShieldNoArg
  nsDialogs::Show
FunctionEnd

; Shows the UAC shield on Next while "All users" is selected and the
; installer is not elevated yet.
Function ModePageUpdateShield
  Pop $0 ; clicked control, when called from NSD_OnClick
  ${NSD_GetState} $ModeRadioAll $1
  StrCpy $2 0
  ${If} $1 == ${BST_CHECKED}
    Call IsAdmin
    Pop $3
    ${If} $3 != 1
      StrCpy $2 1
    ${EndIf}
  ${EndIf}
  GetDlgItem $0 $HWNDPARENT 1
  SendMessage $0 ${BCM_SETSHIELD} 0 $2
FunctionEnd

Function ModePageLeave
  ; Clear the shield so it does not linger on later pages' Next button.
  GetDlgItem $0 $HWNDPARENT 1
  SendMessage $0 ${BCM_SETSHIELD} 0 0

  ${NSD_GetState} $ModeRadioAll $1
  ${If} $1 == ${BST_CHECKED}
    StrCpy $R0 "AllUsers"
    Call IsAdmin
    Pop $2
    ${If} $2 != 1
      Call RelaunchElevated
      ${If} ${Errors}
        MessageBox MB_OK|MB_ICONEXCLAMATION "$(MsgElevationFailed)"
        Call ModePageUpdateShieldNoArg
        Abort
      ${EndIf}
      ; The elevated copy takes over from the directory page.
      Quit
    ${EndIf}
  ${Else}
    StrCpy $R0 "CurrentUser"
  ${EndIf}
  Call ApplyMode
FunctionEnd

Function ModePageUpdateShieldNoArg
  Push 0
  Call ModePageUpdateShield
FunctionEnd

Function DirectoryPageShow
  ; The elevated copy starts here; going Back would reach pages it skips.
  ${If} $Relaunched == 1
    GetDlgItem $0 $HWNDPARENT 3
    EnableWindow $0 0
  ${EndIf}
FunctionEnd

Function LaunchApp
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
FunctionEnd

Function un.onInit
  SetRegView 64
  ; The uninstaller knows where it lives; pick the mode that owns this
  ; directory instead of guessing, since both modes may be installed.
  StrCpy $R9 $INSTDIR
  ReadRegStr $R8 HKLM "${INSTALL_KEY}" "InstallDir"
  ${If} $R8 == $R9
    Call un.IsAdmin
    Pop $0
    ${If} $0 != 1
      ${GetParameters} $R1
      ClearErrors
      ${GetOptions} $R1 "/RELAUNCHED" $R2
      ${IfNot} ${Errors}
        MessageBox MB_OK|MB_ICONSTOP "$(MsgUninstallNeedsAdmin)" /SD IDOK
        SetErrorLevel 740
        Quit
      ${EndIf}
      ; This process is a temporary copy; relaunch the installed
      ; uninstaller elevated so it makes its own copy.
      StrCpy $1 ""
      ${If} ${Silent}
        StrCpy $1 "/S "
      ${EndIf}
      ClearErrors
      ExecShell "runas" "$R9\uninstall.exe" '$1/AllUsers /RELAUNCHED'
      ${If} ${Errors}
        MessageBox MB_OK|MB_ICONSTOP "$(MsgUninstallNeedsAdmin)" /SD IDOK
        SetErrorLevel 740
      ${EndIf}
      Quit
    ${EndIf}
    Call un.UseAllUsers
  ${Else}
    Call un.UseCurrentUser
  ${EndIf}
  StrCpy $INSTDIR $R9
FunctionEnd

Section "Install"
  !insertmacro WaitForAppExit

  SetOutPath "$INSTDIR"
  File "/oname=${APP_EXE}" "${SOURCE_EXE}"
  File "/oname=LICENSE.txt" "..\..\LICENSE"
  File "/oname=${APP_ICON}" "..\..\assets\icons\${APP_ICON}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; SetShellVarContext (set by ApplyMode) points these at the current user's
  ; or the shared Start menu/Desktop; SHCTX is HKCU or HKLM accordingly.
  ; The exe carries no icon resource, so the shortcuts point at the .ico.
  CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_ICON}" 0
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_ICON}" 0

  WriteRegStr SHCTX "${INSTALL_KEY}" "InstallDir" "$INSTDIR"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "URLInfoAbout" "${APP_URL}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_ICON}"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe" /$InstallMode'
  WriteRegStr SHCTX "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /$InstallMode /S'
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
  Delete "$INSTDIR\${APP_ICON}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey SHCTX "${UNINSTALL_KEY}"
  DeleteRegKey SHCTX "${INSTALL_KEY}"
  DeleteRegKey /ifempty SHCTX "Software\Rusker"
SectionEnd
