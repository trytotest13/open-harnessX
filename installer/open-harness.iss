; Inno Setup script for Open Harness
; Build: ISCC.exe open-harness.iss  ->  Output\OpenHarness-<ver>-setup.exe
; Bundles the supervisor exe, the llama.cpp CUDA engine, the UI, and skills;
; checks/installs the WebView2 runtime; seeds config.json for the install dir.

#define AppName "Open Harness"
#define AppVersion "0.2.0"
#define AppPublisher "Open Harness"

[Setup]
AppId={{8E5A6F3E-2C9B-4A7D-9E1F-0B4C7A5D6E81}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\Open Harness
DefaultGroupName=Open Harness
PrivilegesRequiredOverridesAllowed=dialog commandline
UninstallDisplayIcon={app}\open-harness.exe
SetupIconFile=..\launcher\assets\open-harness.ico
WizardStyle=modern
Compression=lzma2/normal
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=OpenHarness-{#AppVersion}-setup
VersionInfoCompany={#AppPublisher}
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#AppVersion}
DisableProgramGroupPage=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\open-harness.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\launcher\ui\index.html"; DestDir: "{app}\launcher\ui"; Flags: ignoreversion
Source: "..\skills\*.md"; DestDir: "{app}\skills"; Flags: ignoreversion
Source: "config.default.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\engine\*"; DestDir: "{app}\engine"; Excludes: "*.zip,*.log"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "wv2-boot.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall

[Dirs]
Name: "{app}\models"

[Icons]
Name: "{group}\Open Harness"; Filename: "{app}\open-harness.exe"; Parameters: "app"; WorkingDir: "{app}"
Name: "{group}\Uninstall Open Harness"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Open Harness"; Filename: "{app}\open-harness.exe"; Parameters: "app"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\open-harness.exe"; Parameters: "app"; Description: "{cm:LaunchProgram,Open Harness}"; Flags: nowait postinstall skipifsilent

[Code]
// A running instance would hold the exe, so stop it before an upgrade.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var rc: Integer;
begin
  Result := '';
  Exec(ExpandConstant('{cmd}'), '/C taskkill /IM open-harness.exe /F /T >nul 2>&1', '', SW_HIDE, ewWaitUntilTerminated, rc);
  Exec(ExpandConstant('{cmd}'), '/C taskkill /IM llama-server.exe /F /T >nul 2>&1', '', SW_HIDE, ewWaitUntilTerminated, rc);
end;

function WebView2Installed: Boolean;
var pv: String;
begin
  Result := False;
  if RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', pv) or
     RegQueryStringValue(HKCU, 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', pv) then
    Result := (pv <> '') and (pv <> '0.0.0.0');
end;

procedure CurStepChanged(CurStep: TSetupStep);
var rc: Integer; app, src, dst, s, jsonApp: String; lines: TArrayOfString; i: Integer;
begin
  if CurStep = ssPostInstall then begin
    // prerequisite: the WebView2 runtime backs the desktop window (built into Windows 11)
    if not WebView2Installed() then begin
      if MsgBox('Open Harness needs the Microsoft WebView2 runtime, which is missing on this PC. Install it now (internet required)?',
                mbConfirmation, MB_YESNO) = IDYES then
        Exec(ExpandConstant('{tmp}\wv2-boot.exe'), '/silent /install', '', SW_SHOW, ewWaitUntilTerminated, rc);
    end;
    // seed config.json for this install folder; never overwrite an existing one.
    // The path goes into a JSON string, so backslashes must be doubled.
    app := ExpandConstant('{app}');
    src := app + '\config.default.json';
    dst := app + '\config.json';
    if not FileExists(dst) then begin
      s := '';
      LoadStringsFromFile(src, lines);
      for i := 0 to GetArrayLength(lines)-1 do
        s := s + lines[i] + #13#10;
      jsonApp := app;
      StringChangeEx(jsonApp, '\', '\\', True);
      StringChangeEx(s, '@APP@', jsonApp, True);
      SaveStringToFile(dst, s, False);
    end;
  end;
end;
