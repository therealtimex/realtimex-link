; RealTimeX Link's Windows installer (NSIS). It carries the (signed) release executables and
; installs them with Link's own installer, `rtx-linkd install`, into C:\Program Files\RealTimeX Link,
; where RealTimeX's installer puts Link too: never a downgrade, and a running older Link is stopped
; first. That installer replaces the whole folder, so the uninstaller lives in ProgramData.
;
; makensis /DVERSION=0.1.4 /DPAYLOAD=<folder with the .exe files> /DOUTFILE=<setup.exe> installer.nsi

Unicode true
!include "MUI2.nsh"
!include "x64.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !error "VERSION is required"
!endif
!ifndef PAYLOAD
  !error "PAYLOAD is required"
!endif
!ifndef OUTFILE
  !define OUTFILE "RealTimeX-Link-Setup-${VERSION}.exe"
!endif

!define APP "RealTimeX Link"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\RealTimeX Link"

Name "${APP}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APP}"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
BrandingText "${APP} ${VERSION}"
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "CompanyName" "RealTimeX"
VIAddVersionKey "FileDescription" "${APP} Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "RealTimeX"

!define MUI_ICON "..\..\assets\icon.ico"
!define MUI_UNICON "..\..\assets\icon.ico"
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open ${APP}"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenLink
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "${APP} needs 64-bit Windows."
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

; Opened as the signed-in user, not elevated like this installer: through Explorer.
Function OpenLink
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\rtx-link.exe"'
FunctionEnd

Section "Install"
  SetShellVarContext all
  InitPluginsDir
  SetOutPath "$PLUGINSDIR\payload"
  File "${PAYLOAD}\rtx-linkd.exe"
  File "${PAYLOAD}\rtx-link.exe"
  File "${PAYLOAD}\rtx-control.exe"
  DetailPrint "Installing ${APP} ${VERSION}"
  nsExec::ExecToLog '"$PLUGINSDIR\payload\rtx-linkd.exe" install "$INSTDIR"'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "${APP} could not be installed (rtx-linkd install: $0)."
    Abort
  ${EndIf}
  SetOutPath "$TEMP"

  CreateDirectory "$APPDATA\${APP}"
  WriteUninstaller "$APPDATA\${APP}\Uninstall ${APP}.exe"
  CreateShortCut "$SMPROGRAMS\${APP}.lnk" "$INSTDIR\rtx-link.exe" "" "$INSTDIR\rtx-link.exe" 0

  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "RealTimeX"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\rtx-link.exe"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$APPDATA\${APP}\Uninstall ${APP}.exe"'
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  SetShellVarContext all
  SetRegView 64
  ; Stop Link (its window and icon, the daemon, the control helper) before removing it.
  nsExec::Exec 'taskkill /F /IM rtx-link.exe'
  nsExec::Exec 'taskkill /F /IM rtx-linkd.exe'
  nsExec::Exec 'taskkill /F /IM rtx-control.exe'
  Sleep 1000
  ; Start at login, for the user removing it (others' entries point at a program that is gone).
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${APP}"
  RMDir /r "$PROGRAMFILES64\${APP}"
  Delete "$SMPROGRAMS\${APP}.lnk"
  DeleteRegKey HKLM "${UNINSTALL_KEY}"
  Delete "$APPDATA\${APP}\Uninstall ${APP}.exe"
  RMDir "$APPDATA\${APP}"
SectionEnd
