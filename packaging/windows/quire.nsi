; Quire installer (NSIS 3). Per-user install: no admin rights, no UAC prompt.
;
;   makensis -DVERSION=0.4.0 -DSRCDIR=<folder with quire.exe and quire-cli.exe> -DOUTFILE=<installer.exe> quire.nsi
;
; -DENGINE (the full edition) also installs quire-ai-engine.exe from SRCDIR,
; next to quire.exe. Without it (Quire Lite) an engine left by the full
; edition is removed: Lite downloads its own when on-device AI is turned on.
;
; Works with makensis on Windows (CI) and on Linux (cross builds).

Unicode true
; A 64-bit installer when this NSIS build has amd64 stubs (Debian/Ubuntu do);
; the official Windows NSIS only ships x86 stubs, which also work.
!if /FileExists "${NSISDIR}/Stubs/lzma_solid-amd64-unicode"
  Target amd64-unicode
!endif
SetCompressor /SOLID lzma
RequestExecutionLevel user

!ifndef VERSION
  !error "Pass -DVERSION=x.y.z"
!endif
!ifndef SRCDIR
  !error "Pass -DSRCDIR=<folder with the built executables>"
!endif
!ifndef OUTFILE
  !define OUTFILE "Quire-${VERSION}-windows-x64-setup.exe"
!endif

!define APP "Quire"
; "Quire" or "Quire Lite": shown in the installer; the app installs to the same
; place either way, so one edition replaces the other.
!ifndef BRAND
  !define BRAND "Quire"
!endif
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quire"

Name "${BRAND}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Quire"
InstallDirRegKey HKCU "Software\Quire" "InstallDir"
BrandingText "${BRAND} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APP} installer"
VIAddVersionKey "LegalCopyright" "© 2026 Quire contributors · AGPL-3.0-or-later"

!include "MUI2.nsh"
!define MUI_ICON "${__FILEDIR__}/../icons/quire.ico"
!define MUI_UNICON "${__FILEDIR__}/../icons/quire.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\quire.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start Quire"

!insertmacro MUI_PAGE_WELCOME
!define MUI_LICENSEPAGE_TEXT_TOP "Quire is free software under the GNU AGPL v3 or later."
!insertmacro MUI_PAGE_LICENSE "${SRCDIR}\LICENSE.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Quire" SecApp
  SectionIn RO
  ; Close a running copy so files can be replaced on upgrade.
  nsExec::Exec 'taskkill /IM quire.exe /F'
  nsExec::Exec 'taskkill /IM quire-ai-engine.exe /F'
  SetOutPath "$INSTDIR"
  File "${SRCDIR}\quire.exe"
  File "${SRCDIR}\quire-cli.exe"
!ifdef ENGINE
  File "${SRCDIR}\quire-ai-engine.exe"
!else
  Delete "$INSTDIR\quire-ai-engine.exe"
!endif
  File /oname=README.md "${SRCDIR}\README.md"
  File "${SRCDIR}\LICENSE.txt"
  WriteUninstaller "$INSTDIR\Uninstall Quire.exe"

  CreateShortcut "$SMPROGRAMS\Quire.lnk" "$INSTDIR\quire.exe"

  WriteRegStr HKCU "Software\Quire" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${BRAND}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "Quire"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\quire.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\Uninstall Quire.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall Quire.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" 20000
SectionEnd

; "Open with Quire" for .md files (0.13). Registers Quire as a program a file
; can be opened with, and in Settings → Default apps; it never makes Quire
; the default (Windows keeps whatever the person chose, and 10/11 only let
; the person change it). Only per-user keys (HKCU), like the rest of the
; installer. Only .md: Quire doesn't open .markdown files
; (docs/design/0.13-pack.md §3).
Section "Offer Quire for .md files" SecAssoc
  WriteRegStr HKCU "Software\Classes\Quire.Markdown" "" "Markdown document"
  WriteRegStr HKCU "Software\Classes\Quire.Markdown\DefaultIcon" "" "$INSTDIR\quire.exe,0"
  WriteRegStr HKCU "Software\Classes\Quire.Markdown\shell\open\command" "" '"$INSTDIR\quire.exe" "%1"'
  ; An entry in the .md "Open with" list. An empty value under OpenWithProgids
  ; adds a choice; the key's own default value (the current default) is untouched.
  WriteRegStr HKCU "Software\Classes\.md\OpenWithProgids" "Quire.Markdown" ""
  WriteRegStr HKCU "Software\Classes\Applications\quire.exe" "FriendlyAppName" "Quire"
  WriteRegStr HKCU "Software\Classes\Applications\quire.exe\SupportedTypes" ".md" ""
  WriteRegStr HKCU "Software\Classes\Applications\quire.exe\shell\open\command" "" '"$INSTDIR\quire.exe" "%1"'
  ; Settings → Default apps lists Quire (as a choice for .md).
  WriteRegStr HKCU "Software\Quire\Capabilities" "ApplicationName" "Quire"
  WriteRegStr HKCU "Software\Quire\Capabilities" "ApplicationDescription" "Fast, local-first Markdown notes"
  WriteRegStr HKCU "Software\Quire\Capabilities\FileAssociations" ".md" "Quire.Markdown"
  WriteRegStr HKCU "Software\RegisteredApplications" "Quire" "Software\Quire\Capabilities"
  ; SHCNE_ASSOCCHANGED: Explorer re-reads the associations.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\Quire.lnk" "$INSTDIR\quire.exe"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} "The Quire app and quire-cli (search, import and the MCP server for AI assistants)."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAssoc} "List Quire under Open with for .md files. It doesn't become the default: you choose that in Windows."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Add a Quire shortcut to the desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  nsExec::Exec 'taskkill /IM quire.exe /F'
  nsExec::Exec 'taskkill /IM quire-ai-engine.exe /F'
  Delete "$INSTDIR\quire.exe"
  Delete "$INSTDIR\quire-cli.exe"
  Delete "$INSTDIR\quire-ai-engine.exe"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\Uninstall Quire.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Quire.lnk"
  Delete "$DESKTOP\Quire.lnk"
  DeleteRegKey HKCU "${UNINST_KEY}"
  ; Only what the installer added: another program's .md choices and the
  ; default stay as they are.
  DeleteRegKey HKCU "Software\Classes\Quire.Markdown"
  DeleteRegKey HKCU "Software\Classes\Applications\quire.exe"
  DeleteRegValue HKCU "Software\Classes\.md\OpenWithProgids" "Quire.Markdown"
  DeleteRegValue HKCU "Software\RegisteredApplications" "Quire"
  DeleteRegKey HKCU "Software\Quire"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
  ; Notes and settings are never touched: vaults are your own folders.
SectionEnd
