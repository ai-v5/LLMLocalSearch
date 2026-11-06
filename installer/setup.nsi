Unicode True
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "Sections.nsh"
!ifndef STAGING_DIR
  !error "STAGING_DIR is required"
!endif
!ifndef OUTPUT_FILE
  !error "OUTPUT_FILE is required"
!endif
Name "LLMLocalSearch"
OutFile "${OUTPUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\LLMLocalSearch"
InstallDirRegKey HKCU "Software\LLMLocalSearch" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID zlib
Icon "${STAGING_DIR}\icon.ico"
UninstallIcon "${STAGING_DIR}\icon.ico"
VIProductVersion "0.1.2.0"
VIAddVersionKey "ProductName" "LLMLocalSearch"
VIAddVersionKey "FileDescription" "LLMLocalSearch Setup"
VIAddVersionKey "ProductVersion" "0.1.2"
VIAddVersionKey "FileVersion" "0.1.2.0"
VIAddVersionKey "LegalCopyright" "Copyright ai-v5 contributors"
!define MUI_ABORTWARNING
!define MUI_ICON "${STAGING_DIR}\icon.ico"
!define MUI_UNICON "${STAGING_DIR}\icon.ico"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGING_DIR}\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_COMPONENTS
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

Section "Install"
  FindWindow $0 "" "LLMLocalSearch"
  StrCmp $0 0 install_continue
  MessageBox MB_OK|MB_ICONEXCLAMATION "请先从系统托盘退出 LLMLocalSearch，再继续安装。" /SD IDOK
  SetErrorLevel 1
  Abort
install_continue:
  ReadRegStr $1 HKCU "Software\LLMLocalSearch" "InstallDir"
  StrCmp $1 "" fresh_install
  StrCmp $1 $INSTDIR 0 install_conflict
  IfFileExists "$INSTDIR\uninstall.exe" install_files install_conflict
fresh_install:
  IfFileExists "$INSTDIR\uninstall.exe" install_conflict
  IfFileExists "$SMPROGRAMS\LLMLocalSearch\LLMLocalSearch.lnk" install_conflict
  IfFileExists "$SMPROGRAMS\LLMLocalSearch\卸载 LLMLocalSearch.lnk" install_conflict
  !include "${STAGING_DIR}\check-collisions.nsh"
install_files:
  !include "${STAGING_DIR}\install-files.nsh"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\LLMLocalSearch" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\LLMLocalSearch" "DisplayName" "LLMLocalSearch"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\LLMLocalSearch" "DisplayVersion" "0.1.2"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\LLMLocalSearch" "Publisher" "ai-v5"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\LLMLocalSearch" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\LLMLocalSearch" "DisplayIcon" "$INSTDIR\llmlocalsearch.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\LLMLocalSearch" "UninstallString" '"$INSTDIR\uninstall.exe"'
  CreateDirectory "$SMPROGRAMS\LLMLocalSearch"
  CreateShortCut "$SMPROGRAMS\LLMLocalSearch\LLMLocalSearch.lnk" "$INSTDIR\llmlocalsearch.exe" "" "$INSTDIR\icon.ico"
  CreateShortCut "$SMPROGRAMS\LLMLocalSearch\卸载 LLMLocalSearch.lnk" "$INSTDIR\uninstall.exe" "" "$INSTDIR\icon.ico"
  Goto install_done
install_conflict:
  MessageBox MB_OK|MB_ICONEXCLAMATION "目录中已有同名文件或现有安装。为避免覆盖数据，请选择其他目录或先卸载现有版本。" /SD IDOK
  SetErrorLevel 1
  Abort
install_done:
SectionEnd

Section /o "un.删除当前索引（旧副本与其他文件保留）" un.DeleteIndex
  FindWindow $0 "" "LLMLocalSearch"
  StrCmp $0 0 uninstall_index_continue
  MessageBox MB_OK|MB_ICONEXCLAMATION "请先从系统托盘退出 LLMLocalSearch，再继续卸载。" /SD IDOK
  SetErrorLevel 1
  Abort
uninstall_index_continue:
  ExecWait '"$INSTDIR\index-maintenance.exe" --delete-current-index "$INSTDIR"' $0
  StrCmp $0 0 uninstall_index_done
  MessageBox MB_OK|MB_ICONEXCLAMATION "无法安全删除当前索引，卸载已停止。" /SD IDOK
  SetErrorLevel 1
  Abort
uninstall_index_done:
SectionEnd

Section "un.卸载程序（默认保留索引和设置）" un.Main
  SectionIn RO
  FindWindow $0 "" "LLMLocalSearch"
  StrCmp $0 0 uninstall_continue
  MessageBox MB_OK|MB_ICONEXCLAMATION "请先从系统托盘退出 LLMLocalSearch，再继续卸载。" /SD IDOK
  SetErrorLevel 1
  Abort
uninstall_continue:
  !include "${STAGING_DIR}\uninstall-files.nsh"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR\Index"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\LLMLocalSearch\LLMLocalSearch.lnk"
  Delete "$SMPROGRAMS\LLMLocalSearch\卸载 LLMLocalSearch.lnk"
  RMDir "$SMPROGRAMS\LLMLocalSearch"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "LLMLocalSearch"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\LLMLocalSearch"
  DeleteRegKey HKCU "Software\LLMLocalSearch"
SectionEnd

Function un.onInit
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/REMOVEINDEX" $1
  IfErrors done
  !insertmacro SelectSection ${un.DeleteIndex}
done:
FunctionEnd
