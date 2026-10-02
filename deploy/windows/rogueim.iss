; RogueIM installer (Inno Setup 6). Built by CI:
;   ISCC.exe /DAppVersion=0.1.4 /DSourceDir=<folder with the release files> /DOutputDir=<dist> rogueim.iss
; Installs per user (no admin rights) into %LOCALAPPDATA%\Programs\RogueIM,
; where RogueIM's own updater can replace its files later.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

[Setup]
AppId={{3AF504C3-9E12-4FD4-8EE6-4FDE7BF241A2}
AppName=RogueIM
AppVersion={#AppVersion}
AppVerName=RogueIM {#AppVersion}
AppPublisher=RogueIM
AppPublisherURL=https://github.com/cyberhirsch/RogueIM
AppSupportURL=https://github.com/cyberhirsch/RogueIM/issues
DefaultDirName={localappdata}\Programs\RogueIM
DefaultGroupName=RogueIM
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
OutputDir={#OutputDir}
OutputBaseFilename=RogueIM-windows-x64-setup
SetupIconFile=..\..\app\assets\icon.ico
UninstallDisplayIcon={app}\rogueim.exe
LicenseFile=..\..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; RogueIM runs while updating itself; the installer closes it first.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\rogueim.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rim-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rim-plugin-pomodoro.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rim-plugin-todo.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rim-plugin-player.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\sounds\*"; DestDir: "{app}\sounds"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\RogueIM"; Filename: "{app}\rogueim.exe"
Name: "{group}\Uninstall RogueIM"; Filename: "{uninstallexe}"
Name: "{autodesktop}\RogueIM"; Filename: "{app}\rogueim.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\rogueim.exe"; Description: "{cm:LaunchProgram,RogueIM}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Leftovers of in-app updates. Profiles (keys, history) stay in %APPDATA%\RogueIM.
Type: files; Name: "{app}\*.old"
Type: files; Name: "{app}\*.new"
