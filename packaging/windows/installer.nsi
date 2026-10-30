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
; Start with this computer (realtimex-link#10): Link's service, LocalSystem. It does nothing until
; the setting is turned on (crates/linkd/src/service.rs).
!define SERVICE "RealTimeX Link Service"
; Lets services send Ctrl+Alt+Delete (SendSAS) for a viewer. Machine policy: the value it had is
; kept in the uninstall key and put back on uninstall.
!define POLICY_KEY "Software\Microsoft\Windows\CurrentVersion\Policies\System"
!define SAS_VALUE "SoftwareSASGeneration"

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
VIAddVersionKey "LegalCopyright" "Copyright © 2025 RealTimeX"

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
    MessageBox MB_ICONSTOP "${APP} needs 64-bit Windows." /SD IDOK
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

; Opened as the signed-in user, not elevated like this installer: through Explorer.
Function OpenLink
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\rtx-link.exe"'
FunctionEnd

; Stop the service before files are replaced (rtx-linkd install also stops Link's processes).
Function StopService
  nsExec::Exec '"$SYSDIR\sc.exe" stop "${SERVICE}"'
  Pop $0
  ${If} $0 == 0
    Sleep 3000
  ${EndIf}
FunctionEnd

; ProgramData\RealTimeX Link holds the service's choice (unattended.json). Before anything is
; written there, and before any file of Link's is replaced (a refusal then leaves nothing half
; installed: QA of !12), rtx-linkd makes it trusted, as the service does at every start: no link,
; administrators as owner, SYSTEM and administrators write, users read, nothing else; a choice
; left in a folder that wasn't already like that is dropped.
Function SecureFolder
  nsExec::ExecToLog '"$PLUGINSDIR\payload\rtx-linkd.exe" secure-machine-dir'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "${APP} could not secure its folder in ProgramData (rtx-linkd: $0)." /SD IDOK
    Abort
  ${EndIf}
FunctionEnd

; The service when it is missing, pointed at what is installed; an existing one is left as it
; is, and so are its running sessions (SD, realtimex-ai-app#2967).
Function EnsureService
  nsExec::Exec '"$SYSDIR\sc.exe" query "${SERVICE}"'
  Pop $0
  ${If} $0 != 0
    nsExec::ExecToLog '"$SYSDIR\sc.exe" create "${SERVICE}" binPath= "\"$INSTDIR\rtx-linkd.exe\" --service" start= auto obj= LocalSystem DisplayName= "${SERVICE}"'
    Call DescribeService
    nsExec::ExecToLog '"$SYSDIR\sc.exe" start "${SERVICE}"'
  ${EndIf}
FunctionEnd

Function DescribeService
  nsExec::ExecToLog '"$SYSDIR\sc.exe" description "${SERVICE}" "Lets computers you allowed reach this one before anyone signs in, when Start with this computer is on."'
  nsExec::ExecToLog '"$SYSDIR\sc.exe" failure "${SERVICE}" reset= 86400 actions= restart/5000/restart/5000/restart/60000'
FunctionEnd

; Ctrl+Alt+Delete from services, keeping what Ease of Access had. The first install records the
; value it found ("none" when there was none). Writing it again changes nothing.
Function EnsureSas
  ClearErrors
  ReadRegStr $1 HKLM "${UNINSTALL_KEY}" "Previous${SAS_VALUE}"
  ${If} ${Errors}
    ClearErrors
    ReadRegDWORD $2 HKLM "${POLICY_KEY}" "${SAS_VALUE}"
    ${If} ${Errors}
      StrCpy $1 "none"
      StrCpy $2 0
    ${Else}
      StrCpy $1 $2
    ${EndIf}
    WriteRegStr HKLM "${UNINSTALL_KEY}" "Previous${SAS_VALUE}" $1
  ${Else}
    ReadRegDWORD $2 HKLM "${POLICY_KEY}" "${SAS_VALUE}"
  ${EndIf}
  IntOp $3 $2 | 1
  ${If} $3 != $2
    WriteRegDWORD HKLM "${POLICY_KEY}" "${SAS_VALUE}" $3
  ${EndIf}
FunctionEnd

; The uninstaller, the Start menu entry and Apps & features' entry, for this version.
Function WriteMetadata
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
FunctionEnd

Section "Install"
  SetShellVarContext all
  InitPluginsDir
  SetOutPath "$PLUGINSDIR\payload"
  File "${PAYLOAD}\rtx-linkd.exe"
  File "${PAYLOAD}\rtx-link.exe"
  File "${PAYLOAD}\rtx-control.exe"
  ; Decide first (rtx-linkd install --check: 0 replaces, 11 is this version, 10 is newer), then
  ; stop the service only when its files will be replaced: stopping it ends live remote
  ; sessions, even at the sign-in screen.
  nsExec::ExecToLog '"$PLUGINSDIR\payload\rtx-linkd.exe" install --check "$INSTDIR"'
  Pop $0
  ${If} $0 == 10
    ; A newer Link is installed: it is left alone.
    DetailPrint "A newer ${APP} is installed: nothing is replaced"
    Call EnsureService
    SetOutPath "$TEMP"
    Return
  ${ElseIf} $0 == 11
    ; This version is installed: nothing is replaced or stopped, and what a failed install
    ; left out is completed (QA of !12).
    DetailPrint "${APP} ${VERSION} is installed: completing what is missing"
    Call SecureFolder
    Call EnsureService
    Call EnsureSas
    ClearErrors
    ReadRegStr $1 HKLM "${UNINSTALL_KEY}" "DisplayVersion"
    ${If} ${Errors}
    ${OrIf} $1 != "${VERSION}"
    ${OrIfNot} ${FileExists} "$APPDATA\${APP}\Uninstall ${APP}.exe"
      Call WriteMetadata
    ${EndIf}
    SetOutPath "$TEMP"
    Return
  ${ElseIf} $0 != 0
    MessageBox MB_ICONSTOP "${APP} could not be installed (rtx-linkd install --check: $0)." /SD IDOK
    Abort
  ${EndIf}
  Call SecureFolder
  Call StopService
  DetailPrint "Installing ${APP} ${VERSION}"
  nsExec::ExecToLog '"$PLUGINSDIR\payload\rtx-linkd.exe" install "$INSTDIR"'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "${APP} could not be installed (rtx-linkd install: $0)." /SD IDOK
    Abort
  ${EndIf}
  SetOutPath "$TEMP"

  ; The service: created, or pointed at this install when it exists; restarted if it fails.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" create "${SERVICE}" binPath= "\"$INSTDIR\rtx-linkd.exe\" --service" start= auto obj= LocalSystem DisplayName= "${SERVICE}"'
  Pop $0
  ${If} $0 != 0
    nsExec::ExecToLog '"$SYSDIR\sc.exe" config "${SERVICE}" binPath= "\"$INSTDIR\rtx-linkd.exe\" --service" start= auto obj= LocalSystem'
  ${EndIf}
  Call DescribeService
  Call EnsureSas
  Call WriteMetadata
  nsExec::ExecToLog '"$SYSDIR\sc.exe" start "${SERVICE}"'
SectionEnd

Section "Uninstall"
  SetShellVarContext all
  SetRegView 64
  ; The service first: it would start the daemon again.
  nsExec::Exec '"$SYSDIR\sc.exe" stop "${SERVICE}"'
  Sleep 2000
  nsExec::Exec '"$SYSDIR\sc.exe" delete "${SERVICE}"'
  ; The SAS policy as it was before Link.
  ClearErrors
  ReadRegStr $1 HKLM "${UNINSTALL_KEY}" "Previous${SAS_VALUE}"
  ${IfNot} ${Errors}
    ${If} $1 == "none"
      DeleteRegValue HKLM "${POLICY_KEY}" "${SAS_VALUE}"
    ${Else}
      WriteRegDWORD HKLM "${POLICY_KEY}" "${SAS_VALUE}" $1
    ${EndIf}
  ${EndIf}
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
  Delete "$APPDATA\${APP}\unattended.json"
  Delete "$APPDATA\${APP}\service.log"
  RMDir /r "$APPDATA\${APP}\updates"
  RMDir /r "$APPDATA\${APP}\transfers"
  RMDir "$APPDATA\${APP}"
SectionEnd
