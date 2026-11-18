; Open Live Ingest for Windows (x86_64): the installer, built by the `windows-app` job
; of .github/workflows/release.yml (docs/specs/desktop-app.md, section 11.2).
;
; Per machine, into %ProgramFiles%\Open Live Ingest: the desktop app, the gateway CLI
; (for `status` and `down` from a console, by full path; PATH is not changed), the two
; licences and the icon. A Start menu shortcut and an entry in Apps & features. Strom
; comes from its own MSI, embedded here and run only when %ProgramFiles%\Strom\bin\strom.exe
; is absent, the place local_strom looks (MSI_STROM); an existing Strom is left alone.
; The uninstaller removes the app, the CLI and the shortcut, and leaves Strom (its own
; entry in Apps & features) and the user's settings, logs, keychain entry and WebView2
; folder.
;
; makensis is given, with /D:
;   VERSION         the release version, e.g. 0.5.0
;   VERSION_QUAD    the same as four numbers, e.g. 0.5.0.0 (VIProductVersion needs it)
;   STAGE           folder with open-live-ingest-app.exe, open-live-ingest.exe,
;                   LICENSE-MIT, LICENSE-APACHE and open-live-ingest.ico
;   STROM_MSI       path to the Strom MSI, already checked against its GitHub digest
;   STROM_MSI_NAME  its file name, strom-<pin>-windows-x86_64.msi
;   OUTFILE         the installer to write
;
; Silent use: `setup.exe /S` installs, `uninstall.exe /S` uninstalls. The exit code is 0
; on success; when the Strom MSI fails it is msiexec's exit code, and when WebView2 is
; missing or Windows is not 64-bit it is 2.

!macro RequireDefine NAME
  !ifndef ${NAME}
    !error "pass ${NAME} to makensis with /D${NAME}=..."
  !endif
!macroend
!insertmacro RequireDefine VERSION
!insertmacro RequireDefine VERSION_QUAD
!insertmacro RequireDefine STAGE
!insertmacro RequireDefine STROM_MSI
!insertmacro RequireDefine STROM_MSI_NAME
!insertmacro RequireDefine OUTFILE

Unicode true
ManifestDPIAware true
; The Strom MSI is most of the payload and already compressed; a solid LZMA block
; still shrinks the rest without a second pass over it.
SetCompressor /SOLID lzma
RequestExecutionLevel admin

!include LogicLib.nsh
!include x64.nsh
!include FileFunc.nsh

!define PRODUCT "Open Live Ingest"
!define PUBLISHER "Eyevinn Technology"
!define APP_EXE "open-live-ingest-app.exe"
!define CLI_EXE "open-live-ingest.exe"
!define ICON_FILE "open-live-ingest.ico"
!define UNINSTALLER "uninstall.exe"
!define SHORTCUT "$SMPROGRAMS\${PRODUCT}.lnk"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\OpenLiveIngest"
; Where the Strom MSI installs strom.exe (Eyevinn/strom installer/windows/Product.wxs).
!define MSI_STROM "$PROGRAMFILES64\Strom\bin\strom.exe"
; The WebView2 Evergreen runtime's client key; `pv` holds its version.
!define WEBVIEW2_CLIENT "Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

Name "${PRODUCT}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${PRODUCT}"
Icon "${STAGE}\${ICON_FILE}"
UninstallIcon "${STAGE}\${ICON_FILE}"
BrandingText "${PRODUCT} ${VERSION}"
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${VERSION_QUAD}"
VIFileVersion "${VERSION_QUAD}"
VIAddVersionKey "ProductName" "${PRODUCT}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${PRODUCT} ${VERSION} installer"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "LegalCopyright" "MIT OR Apache-2.0"

Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

; A missing WebView2 runtime stops the install (docs/specs/desktop-app.md, section 10).
; Per machine on 64-bit Windows it is under WOW6432Node; per user under HKCU.
Function CheckWebView2
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\${WEBVIEW2_CLIENT}" "pv"
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    ReadRegStr $0 HKLM "SOFTWARE\${WEBVIEW2_CLIENT}" "pv"
  ${EndIf}
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    ReadRegStr $0 HKCU "Software\${WEBVIEW2_CLIENT}" "pv"
  ${EndIf}
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    MessageBox MB_OK|MB_ICONSTOP "Microsoft Edge WebView2 is required. Install it from Microsoft, then run this installer again." /SD IDOK
    SetErrorLevel 2
    Quit
  ${EndIf}
  DetailPrint "WebView2 runtime $0"
FunctionEnd

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "${PRODUCT} needs 64-bit (x86_64) Windows." /SD IDOK
    SetErrorLevel 2
    Quit
  ${EndIf}
  SetRegView 64
  SetShellVarContext all
  Call CheckWebView2
FunctionEnd

Section "Strom"
  ${If} ${FileExists} "${MSI_STROM}"
    DetailPrint "Strom is already installed at ${MSI_STROM}; it is left as it is."
  ${Else}
    InitPluginsDir
    SetOutPath "$PLUGINSDIR"
    File "/oname=$PLUGINSDIR\${STROM_MSI_NAME}" "${STROM_MSI}"
    DetailPrint "Installing Strom from ${STROM_MSI_NAME}"
    ExecWait '"$SYSDIR\msiexec.exe" /i "$PLUGINSDIR\${STROM_MSI_NAME}" /qn /norestart /l*v "$TEMP\open-live-ingest-strom-msi.log"' $0
    SetOutPath "$TEMP"
    Delete "$PLUGINSDIR\${STROM_MSI_NAME}"
    ; 3010: installed, a restart is needed to finish.
    ${If} $0 == 3010
      SetRebootFlag true
    ${ElseIf} $0 != 0
      MessageBox MB_OK|MB_ICONSTOP "Strom could not be installed (msiexec exit code $0). Its log is $TEMP\open-live-ingest-strom-msi.log." /SD IDOK
      SetErrorLevel $0
      Quit
    ${EndIf}
    ${IfNot} ${FileExists} "${MSI_STROM}"
      MessageBox MB_OK|MB_ICONSTOP "The Strom installer succeeded but ${MSI_STROM} is missing. Its log is $TEMP\open-live-ingest-strom-msi.log." /SD IDOK
      SetErrorLevel 2
      Quit
    ${EndIf}
  ${EndIf}
SectionEnd

Section "Open Live Ingest"
  SetOutPath "$INSTDIR"
  File "${STAGE}\${APP_EXE}"
  File "${STAGE}\${CLI_EXE}"
  File "${STAGE}\LICENSE-MIT"
  File "${STAGE}\LICENSE-APACHE"
  File "${STAGE}\${ICON_FILE}"
  WriteUninstaller "$INSTDIR\${UNINSTALLER}"

  CreateShortcut "${SHORTCUT}" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${ICON_FILE}" 0

  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${ICON_FILE}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/Eyevinn/open-live-ingest"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\${UNINSTALLER}"'
  WriteRegStr HKLM "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\${UNINSTALLER}" /S'
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "EstimatedSize" "$0"
SectionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
FunctionEnd

; Strom, the settings (%APPDATA%\open-live-ingest), the logs, the keychain entry and the
; WebView2 folder (%LOCALAPPDATA%\open-live-ingest\webview) stay.
Section "Uninstall"
  Delete "${SHORTCUT}"
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${CLI_EXE}"
  Delete "$INSTDIR\LICENSE-MIT"
  Delete "$INSTDIR\LICENSE-APACHE"
  Delete "$INSTDIR\${ICON_FILE}"
  Delete "$INSTDIR\${UNINSTALLER}"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "${UNINSTALL_KEY}"
SectionEnd
