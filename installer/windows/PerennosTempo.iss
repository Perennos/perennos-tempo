; Perennos Tempo for Windows: the installer.
;
; Inno Setup 6.4 or later, the installer that Visual Studio Code ships with, in its modern
; wizard style. Build with build.sh (Linux: the compiler runs under Wine) or build.ps1 (Windows). They
; compile the programs, draw the art and pass the folders below; see README.md.
;
; - Installs for the current user by default: no administrator prompt, into
;   %LOCALAPPDATA%\Programs\Perennos Tempo. "Install for all users" goes to Program Files.
; - Every app starts through tempo-launch.exe, which never opens a console window.
; - Uninstalling removes the programs and their shortcuts, PATH entry, Explorer command and scheduled
;   task. It keeps the vault, Tempo's memory and the logs: they are the user's.

#ifndef AppVersion
  #define AppVersion "2.0.1"
#endif
#ifndef Stage
  #define Stage AddBackslash(SourcePath) + "build\stage"
#endif
#ifndef ArtDir
  #define ArtDir AddBackslash(SourcePath) + "build\art"
#endif
#ifndef OutDir
  #define OutDir AddBackslash(SourcePath) + "build\out"
#endif

#define AppName "Perennos Tempo"
; the full edition has every part; build.sh passes /DEdition=source-available for the one without
; the paid Field and VECTIS
#ifdef Edition
  #define EditionName " (" + Edition + " edition)"
  #define EditionSuffix "-" + Edition
#else
  #define EditionName ""
  #define EditionSuffix ""
#endif
#define Publisher "Perennos"
; the Publisher's web page: Settings > Apps shows these as links (build with /DPublisherURL=... to change it)
#ifndef PublisherURL
  #define PublisherURL "https://perennos.com"
#endif
#define AppIdGuid "833513CA-F2D0-462A-AF67-B1EFE3378DC9"
#define Launch "{app}\bin\tempo-launch.exe"

; The optional apps are offered when their program was built.
#define HaveVault FileExists(Stage + "\tempo-vault.exe")
#define HaveStudio FileExists(Stage + "\tempo-studio.exe")
#define HaveRoom FileExists(Stage + "\tempo-room.exe")
#define HaveField FileExists(Stage + "\tempo-dashboard.exe")
#define HaveCli FileExists(Stage + "\tempo.exe")

; Code signing: build with /DSignTool=<name> after defining that sign tool (ISCC /S...). Unsigned
; otherwise; see README.md.
#ifdef SignTool
  #define SignFlag "sign"
#else
  #define SignFlag ""
#endif

[Setup]
; never change it: it is how Windows knows a new version is this app
AppId={{{#AppIdGuid}}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}{#EditionName}
AppPublisher={#Publisher}
AppPublisherURL={#PublisherURL}
AppSupportURL={#PublisherURL}
AppUpdatesURL={#PublisherURL}
AppCopyright=Copyright 2026 {#Publisher}
VersionInfoVersion={#AppVersion}
VersionInfoCompany={#Publisher}
VersionInfoCopyright=Copyright 2026 {#Publisher}
VersionInfoDescription={#AppName} Setup
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#AppVersion}
; who it is for, and where it goes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
UsePreviousPrivileges=yes
UsedUserAreasWarning=no
DefaultDirName={code:ProgramsRoot}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableWelcomePage=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; the look
WizardStyle=modern
WizardImageFile={#ArtDir}\wizard-large-100.bmp,{#ArtDir}\wizard-large-125.bmp,{#ArtDir}\wizard-large-150.bmp,{#ArtDir}\wizard-large-175.bmp,{#ArtDir}\wizard-large-200.bmp,{#ArtDir}\wizard-large-225.bmp,{#ArtDir}\wizard-large-250.bmp
WizardSmallImageFile={#ArtDir}\wizard-small-100.bmp,{#ArtDir}\wizard-small-125.bmp,{#ArtDir}\wizard-small-150.bmp,{#ArtDir}\wizard-small-175.bmp,{#ArtDir}\wizard-small-200.bmp,{#ArtDir}\wizard-small-225.bmp,{#ArtDir}\wizard-small-250.bmp
SetupIconFile={#ArtDir}\tempo.ico
UninstallDisplayIcon={app}\tempo.ico
UninstallDisplayName={#AppName}{#EditionName}
UninstallFilesDir={app}\uninstall
ShowLanguageDialog=no
; updates in place: running apps are stopped first (tempo-launch stop), then restarted by the user
CloseApplications=force
RestartApplications=no
SetupMutex=PerennosTempoSetup,Global\PerennosTempoSetup
ChangesEnvironment=yes
SetupLogging=yes
; the file
Compression=lzma2/ultra64
SolidCompression=yes
; the 64 MB dictionary needs more memory than the 32-bit compiler has: compress in Inno's 64-bit helper
LZMAUseSeparateProcess=yes
OutputDir={#OutDir}
OutputBaseFilename=PerennosTempo-Setup-{#AppVersion}-x64{#EditionSuffix}
#ifdef SignTool
SignTool={#SignTool}
SignedUninstaller=yes
#endif
; the license page (build.sh and build.ps1 make it: PolyForm Shield, the proprietary parts in the
; full edition, the third-party notices); /DLicenseFile=<file.txt or .rtf>
#ifdef LicenseFile
LicenseFile={#LicenseFile}
#endif

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"

[Messages]
SetupWindowTitle={#AppName} Setup
WelcomeLabel1=Welcome to Perennos Tempo
WelcomeLabel2=This installs [name/ver]: the local agent, and the apps it connects to as its tools on its own.%n%nEverything runs on this computer. Nothing is sent anywhere unless you connect it yourself, and uninstalling never touches your notes.%n%nIt takes about a minute.
FinishedHeadingLabel=Perennos Tempo is installed
ClickFinish=Click Finish to close Setup.
SelectComponentsLabel2=Choose what to install. You can come back and change this at any time: your notes and settings stay as they are.

[Types]
Name: "full"; Description: "Everything (recommended)"
Name: "compact"; Description: "The agent and Tempo Vault"
Name: "custom"; Description: "Let me choose"; Flags: iscustom

[Components]
Name: "agent"; Description: "Tempo Agent: the local AI agent, with these apps as its tools"; Types: full compact custom; Flags: fixed
#if HaveVault
Name: "vault"; Description: "Tempo Vault: your notes as Markdown, Obsidian-compatible, learning every hour"; Types: full compact
#endif
#if HaveStudio
Name: "studio"; Description: "Tempo Studio: mixing and mastering, with REAPER when you ask for it"; Types: full
#endif
#if HaveRoom
Name: "room"; Description: "Tempo Room: private chat rooms where Tempo answers when you write @tempo"; Types: full
#endif
#if HaveField
Name: "field"; Description: "Tempo Field: the Perennos Tempo dashboard"; Types: full
#endif
#if HaveCli
Name: "cli"; Description: "The tempo command for terminals"; Types: full
#endif

[Tasks]
Name: "desktopicon"; Description: "Put Perennos Tempo on the desktop"; GroupDescription: "Shortcuts:"; Flags: unchecked
#if HaveVault
Name: "openvault"; Description: "Add ""Open as Tempo Vault"" to folders in File Explorer"; GroupDescription: "Shortcuts:"; Components: vault
#endif
Name: "addtopath"; Description: "Add the tempo programs to PATH, for terminals"; GroupDescription: "Shortcuts:"
#if HaveVault
Name: "learn"; Description: "Let Tempo learn from your vault every hour (on this computer, no window)"; GroupDescription: "Perennos Tempo:"; Components: vault
#endif
; Ollama is the engine Tempo runs models with. The box is ticked for the person when no Ollama is on
; the computer (see CurPageChanged below) and left unticked when one is there. 1471164488 bytes is the
; Windows x86_64 entry in perennos-tempo-launcher\catalog\runtime.json (about 1.4 GB).
Name: "getollama"; Description: "Download Ollama, the engine Tempo runs models with (about 1.4 GB, checked against a pinned SHA-256)"; GroupDescription: "Perennos Tempo:"; Components: agent; Flags: unchecked
Name: "setupmodels"; Description: "Download models that fit this computer now (several gigabytes, needs Ollama)"; GroupDescription: "Perennos Tempo:"; Components: agent; Flags: unchecked
Name: "startup"; Description: "Start Perennos Tempo in the background when I sign in"; GroupDescription: "Perennos Tempo:"; Flags: unchecked

[Files]
Source: "{#Stage}\tempo-launch.exe"; DestDir: "{app}\bin"; Flags: ignoreversion {#SignFlag}
Source: "{#Stage}\tempo-local-agent.exe"; DestDir: "{app}\bin"; Components: agent; Flags: ignoreversion {#SignFlag}
#if HaveVault
Source: "{#Stage}\tempo-vault.exe"; DestDir: "{app}\bin"; Components: vault; Flags: ignoreversion {#SignFlag}
Source: "{#ArtDir}\tempo-vault.ico"; DestDir: "{app}"; Components: vault; Flags: ignoreversion
#endif
#if HaveStudio
Source: "{#Stage}\tempo-studio.exe"; DestDir: "{app}\bin"; Components: studio; Flags: ignoreversion {#SignFlag}
#endif
#if HaveRoom
Source: "{#Stage}\tempo-room.exe"; DestDir: "{app}\bin"; Components: room; Flags: ignoreversion {#SignFlag}
Source: "{#ArtDir}\tempo-room.ico"; DestDir: "{app}"; Components: room; Flags: ignoreversion
#endif
#if HaveField
Source: "{#Stage}\tempo-dashboard.exe"; DestDir: "{app}\bin"; Components: field; Flags: ignoreversion {#SignFlag}
#endif
#if HaveCli
Source: "{#Stage}\tempo.exe"; DestDir: "{app}\bin"; Components: cli; Flags: ignoreversion {#SignFlag}
#endif
Source: "{#ArtDir}\tempo.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\legal\*"; DestDir: "{app}\legal"; Flags: ignoreversion recursesubdirs skipifsourcedoesntexist
Source: "{#Stage}\examples\*"; DestDir: "{app}\examples"; Flags: ignoreversion recursesubdirs skipifsourcedoesntexist

[InstallDelete]
; "Modify" with an app unticked takes its program away (never anything of yours)
#if HaveVault
Type: files; Name: "{app}\bin\tempo-vault.exe"; Components: not vault
#endif
#if HaveStudio
Type: files; Name: "{app}\bin\tempo-studio.exe"; Components: not studio
#endif
#if HaveRoom
Type: files; Name: "{app}\bin\tempo-room.exe"; Components: not room
#endif
#if HaveField
Type: files; Name: "{app}\bin\tempo-dashboard.exe"; Components: not field
#endif
#if HaveCli
Type: files; Name: "{app}\bin\tempo.exe"; Components: not cli
#endif

[Icons]
Name: "{group}\{#AppName}"; Filename: "{#Launch}"; WorkingDir: "{app}"; IconFilename: "{app}\tempo.ico"; Comment: "Open Perennos Tempo"
#if HaveVault
Name: "{group}\Tempo Vault"; Filename: "{#Launch}"; Parameters: "vault"; WorkingDir: "{app}"; IconFilename: "{app}\tempo-vault.ico"; Comment: "Your notes"; Components: vault
#endif
#if HaveStudio
; no Studio icon is staged (the art makes tempo, vault and room icons only); Studio's mark is Tempo's orange P, so tempo.ico
Name: "{group}\Tempo Studio"; Filename: "{#Launch}"; Parameters: "studio"; WorkingDir: "{app}"; IconFilename: "{app}\tempo.ico"; Comment: "Mixing and mastering, through Tempo"; Components: studio
#endif
#if HaveRoom
Name: "{group}\Tempo Room"; Filename: "{#Launch}"; Parameters: "room"; WorkingDir: "{app}"; IconFilename: "{app}\tempo-room.ico"; Comment: "Your chat rooms"; Components: room
#endif
Name: "{autodesktop}\{#AppName}"; Filename: "{#Launch}"; WorkingDir: "{app}"; IconFilename: "{app}\tempo.ico"; Comment: "Open Perennos Tempo"; Tasks: desktopicon

[Registry]
; PATH: the user's (or, for all users, the machine's); taken out again on uninstall in [Code]
Root: HKA; Subkey: "{code:EnvKey}"; ValueType: expandsz; ValueName: "Path"; ValueData: "{code:PathWithBin}"; Tasks: addtopath; Check: NeedsAddPath(ExpandConstant('{app}\bin'))
; where Tempo's things are, for the agent and the command line too. TEMPO_AGENT_DATA is <the Tempo home>\agent,
; the home found by the one rule in [Code] (TempoHome), so the agent and tempo-launch use the same folder.
#if HaveVault
Root: HKCU; Subkey: "Environment"; ValueType: string; ValueName: "TEMPO_VAULT"; ValueData: "{code:VaultDir}"; Components: vault; Flags: uninsdeletevalue; Check: UserEnvMissing('TEMPO_VAULT')
Root: HKCU; Subkey: "Environment"; ValueType: string; ValueName: "TEMPO_VAULT"; ValueData: "{code:VaultDir}"; Components: vault; Check: VaultEnvMoved
#endif
Root: HKCU; Subkey: "Environment"; ValueType: string; ValueName: "TEMPO_AGENT_DATA"; ValueData: "{code:TempoAgentData}"; Flags: uninsdeletevalue; Check: UserEnvMissing('TEMPO_AGENT_DATA')
#if HaveVault
; File Explorer: "Open as Tempo Vault" on a folder, and inside one
Root: HKA; Subkey: "Software\Classes\Directory\shell\PerennosTempoVault"; ValueType: string; ValueName: ""; ValueData: "Open as Tempo Vault"; Tasks: openvault; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Directory\shell\PerennosTempoVault"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\tempo-vault.ico"""; Tasks: openvault
Root: HKA; Subkey: "Software\Classes\Directory\shell\PerennosTempoVault\command"; ValueType: string; ValueName: ""; ValueData: """{#Launch}"" vault ""%V"""; Tasks: openvault
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\PerennosTempoVault"; ValueType: string; ValueName: ""; ValueData: "Open as Tempo Vault"; Tasks: openvault; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\PerennosTempoVault"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\tempo-vault.ico"""; Tasks: openvault
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\PerennosTempoVault\command"; ValueType: string; ValueName: ""; ValueData: """{#Launch}"" vault ""%V"""; Tasks: openvault
#endif
; start in the background at sign-in (this user)
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{#Launch}"" start"; Tasks: startup; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "{#AppName}"; Flags: deletevalue; Tasks: not startup

[Run]
#if HaveVault
; a new vault gets its folder, its settings and a first note; an existing one (Obsidian too) is left as it is
Filename: "{app}\bin\tempo-vault.exe"; Parameters: "init ""{code:VaultDir}"""; Flags: runhidden waituntilterminated; Components: vault; Check: VaultNeedsInit; StatusMsg: "Making your vault..."
Filename: "{#Launch}"; Parameters: "schedule"; Flags: runhidden waituntilterminated; Tasks: learn; StatusMsg: "Setting up hourly learning..."
Filename: "{#Launch}"; Parameters: "schedule --remove"; Flags: runhidden waituntilterminated; Tasks: not learn
#endif
; Ollama, when its box is ticked (ticking it is the consent): fetch only Ollama, checked against the
; pinned SHA-256 before it is unpacked, and start it once to see that it answers. `setup --ollama` never
; looks up or pulls a model and never writes the agent's settings.
Filename: "{#Launch}"; Parameters: "setup --ollama --yes"; Flags: runhidden waituntilterminated; Components: agent; Tasks: getollama; StatusMsg: "Downloading Ollama (checked before it runs)..."
; Models for this computer. The box is unticked by default and says it downloads several gigabytes, so
; no model is ever fetched unless the person ticked it. Exactly one of these three runs:
; 1. box ticked and an Ollama is there (installed, or just set up by the Ollama box): look at the
;    computer and fetch what fits, hidden (`--yes` because ticking the box is the consent);
; 2. box ticked, no Ollama anywhere: only write the plan to <home>\setup.html;
; 3. box not ticked: only write the plan too, so the page exists. In both 2 and 3 the launcher opens
;    the plan at the first start; no model is downloaded.
Filename: "{#Launch}"; Parameters: "setup --apply --yes"; Flags: runhidden waituntilterminated; Components: agent; Tasks: setupmodels; Check: OllamaAvailable; StatusMsg: "Downloading models for this computer. This can take a while..."
Filename: "{#Launch}"; Description: "Open Perennos Tempo"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "{#Launch}"; Parameters: "stop"; Flags: runhidden waituntilterminated; RunOnceId: "StopApps"
Filename: "{#Launch}"; Parameters: "schedule --remove"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveLearn"

[Code]
const
  UserEnvKey = 'Environment';
  MachineEnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

var
  VaultPage: TInputDirWizardPage;
  KeptVault: String;
  OllamaTaskDecided: Boolean;

function SetEnvironmentVariable(lpName: String; lpValue: String): BOOL;
  external 'SetEnvironmentVariableW@kernel32.dll stdcall';

{ Ollama is installed for this user (its own installer's folder), for all users (Program Files), or is
  on PATH. (Tempo's own copy, fetched by the Ollama box, lives in the Tempo folder and is not counted
  here.) }
function OllamaPresent: Boolean;
begin
  Result := FileExists(ExpandConstant('{localappdata}\Programs\Ollama\ollama.exe'))
    or FileExists(ExpandConstant('{commonpf}\Ollama\ollama.exe'))
    or (FileSearch('ollama.exe', GetEnv('PATH')) <> '');
end;

{ An Ollama is there for the models step: one that is installed, or Tempo's own, which the Ollama box
  has just fetched (that step runs first). }
function OllamaAvailable: Boolean;
begin
  Result := OllamaPresent or WizardIsTaskSelected('getollama');
end;

{ Program Files for all users; for one user the per-user Programs folder, which is
  %LOCALAPPDATA%\Programs unless the system has moved it. A system that cannot say where it is
  (some locked-down setups, Wine) gets that usual place. }
function ProgramsRoot(Param: String): String;
begin
  if IsAdminInstallMode then
    Result := ExpandConstant('{autopf}')
  else
    try
      Result := ExpandConstant('{userpf}');
    except
      Result := ExpandConstant('{localappdata}\Programs');
    end;
end;

{ ---- environment }

function EnvRoot: Integer;
begin
  if IsAdminInstallMode then
    Result := HKEY_LOCAL_MACHINE
  else
    Result := HKEY_CURRENT_USER;
end;

function EnvKey(Param: String): String;
begin
  if IsAdminInstallMode then
    Result := MachineEnvKey
  else
    Result := UserEnvKey;
end;

function UserEnv(Name: String): String;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, UserEnvKey, Name, Result) then
    Result := '';
end;

function UserEnvMissing(Name: String): Boolean;
begin
  Result := UserEnv(Name) = '';
end;

{ ---- the Tempo home: the same rule as tempo-launch, Room, the CLI and the agent (docs/TEMPO-HOME.md)
  1. TEMPO_HOME, if set and not empty; PERENNOS_TEMPO_DATA (the older name) second.
  2. Else the folder named by TEMPO_LEGACY_HOME (where an earlier Perennos Tempo kept its data),
     only if that folder already exists.
  3. Else %LOCALAPPDATA%\Perennos Tempo.
  Nothing is created or moved here. The variables are read from this process, else from the user's
  saved environment (a value with a %VAR% in it is not expanded, so it is ignored there). }
function EnvValue(Name: String): String;
begin
  Result := GetEnv(Name);
  if Result = '' then
  begin
    Result := UserEnv(Name);
    if Pos('%', Result) > 0 then
      Result := '';
  end;
end;

function TempoHome: String;
begin
  Result := RemoveBackslashUnlessRoot(Trim(EnvValue('TEMPO_HOME')));
  if Result = '' then
    Result := RemoveBackslashUnlessRoot(Trim(EnvValue('PERENNOS_TEMPO_DATA')));
  if Result = '' then
  begin
    Result := RemoveBackslashUnlessRoot(Trim(EnvValue('TEMPO_LEGACY_HOME')));
    if (Result = '') or (not DirExists(Result)) then
      Result := ExpandConstant('{localappdata}\{#AppName}');
  end;
end;

function TempoAgentData(Param: String): String;
begin
  Result := TempoHome + '\agent';
end;

function PathHas(Paths, Dir: String): Boolean;
begin
  Result := (Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0)
    or (Pos(';' + Uppercase(Dir) + '\;', ';' + Uppercase(Paths) + ';') > 0);
end;

function NeedsAddPath(Dir: String): Boolean;
var
  Paths: String;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey(''), 'Path', Paths) then
    Paths := '';
  Result := not PathHas(Paths, Dir);
end;

// the Path as it is (variables unexpanded), with the app's bin folder at the end
function PathWithBin(Param: String): String;
var
  Paths: String;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey(''), 'Path', Paths) then
    Paths := '';
  Result := ExpandConstant('{app}\bin');
  if Paths <> '' then
    if Copy(Paths, Length(Paths), 1) = ';' then
      Result := Paths + Result
    else
      Result := Paths + ';' + Result;
end;

procedure RemoveFromPath(Dir: String);
var
  Paths, Rest, Item, Kept: String;
  P: Integer;
  First: Boolean;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey(''), 'Path', Paths) then
    exit;
  Rest := Paths + ';';
  Kept := '';
  First := True;
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    Item := Copy(Rest, 1, P - 1);
    Rest := Copy(Rest, P + 1, Length(Rest));
    if CompareText(RemoveBackslashUnlessRoot(Trim(Item)), RemoveBackslashUnlessRoot(Dir)) <> 0 then
    begin
      if not First then
        Kept := Kept + ';';
      Kept := Kept + Item;
      First := False;
    end;
  end;
  if Kept <> Paths then
    RegWriteExpandStringValue(EnvRoot, EnvKey(''), 'Path', Kept);
end;

{ ---- the vault }

function DefaultVault: String;
begin
  { /VAULT="D:\Notes" on the command line, else the last choice, else TEMPO_VAULT, else Documents }
  Result := ExpandConstant('{param:VAULT|}');
  if Result = '' then
    Result := GetPreviousData('VaultDir', '');
  if Result = '' then
    Result := UserEnv('TEMPO_VAULT');
  if Result = '' then
    Result := ExpandConstant('{userdocs}\Tempo Vault');
end;

function VaultDir(Param: String): String;
begin
  if VaultPage <> nil then
    Result := RemoveBackslashUnlessRoot(Trim(VaultPage.Values[0]))
  else
    Result := DefaultVault;
end;

function VaultEnvMoved: Boolean;
begin
  Result := (UserEnv('TEMPO_VAULT') <> '') and (CompareText(RemoveBackslashUnlessRoot(UserEnv('TEMPO_VAULT')), VaultDir('')) <> 0);
end;

function IsEmptyDir(Dir: String): Boolean;
var
  F: TFindRec;
begin
  Result := True;
  if FindFirst(AddBackslash(Dir) + '*', F) then
  try
    repeat
      if (F.Name <> '.') and (F.Name <> '..') then
      begin
        Result := False;
        break;
      end;
    until not FindNext(F);
  finally
    FindClose(F);
  end;
end;

function VaultNeedsInit: Boolean;
begin
  Result := (not DirExists(VaultDir(''))) or IsEmptyDir(VaultDir(''));
end;

procedure InitializeWizard;
begin
#if HaveVault
  VaultPage := CreateInputDirPage(wpSelectComponents,
    'Your vault',
    'Where should Tempo Vault keep your notes?',
    'Your notes are plain Markdown files in a folder you choose. Pick a new folder, or an Obsidian vault you already have: it opens just as it is, and Obsidian keeps working with it.' + #13#10#13#10 +
    'Tempo reads and learns from this folder. Nothing in it leaves this computer.',
    False, 'Tempo Vault');
  VaultPage.Add('');
  VaultPage.Values[0] := DefaultVault;
#endif
end;

procedure RegisterPreviousData(PreviousDataKey: Integer);
begin
  if VaultPage <> nil then
    SetPreviousData(PreviousDataKey, 'VaultDir', VaultDir(''));
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := False;
  if (VaultPage <> nil) and (PageID = VaultPage.ID) then
    Result := not WizardIsComponentSelected('vault');
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (VaultPage <> nil) and (CurPageID = VaultPage.ID) and (VaultDir('') = '') then
  begin
    MsgBox('Choose a folder for your vault.', mbError, MB_OK);
    Result := False;
  end;
end;

function UpdateReadyMemo(Space, NewLine, MemoUserInfoInfo, MemoDirInfo, MemoTypeInfo, MemoComponentsInfo, MemoGroupInfo, MemoTasksInfo: String): String;
begin
  Result := MemoDirInfo + NewLine + NewLine;
  if MemoTypeInfo <> '' then
    Result := Result + MemoTypeInfo + NewLine + NewLine;
  if MemoComponentsInfo <> '' then
    Result := Result + MemoComponentsInfo + NewLine + NewLine;
  if (VaultPage <> nil) and WizardIsComponentSelected('vault') then
  begin
    Result := Result + 'Your vault:' + NewLine + Space + VaultDir('');
    if not VaultNeedsInit then
      Result := Result + '  (already has notes: left as it is)';
    Result := Result + NewLine + NewLine;
  end;
  if MemoTasksInfo <> '' then
    Result := Result + MemoTasksInfo + NewLine + NewLine;
  Result := Result + 'Tempo''s memory and logs:' + NewLine + Space + TempoHome;
end;

{ An older version still running is stopped before its programs are replaced. tempo-launch keeps its
  run records in <Tempo home>\run, so it is asked in two places: the home found now, and the folder an
  earlier version used (%LOCALAPPDATA%\Perennos Tempo), through TEMPO_LAUNCH_HOME, which only
  tempo-launch reads. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
  Old: String;
begin
  Result := '';
  if FileExists(ExpandConstant('{#Launch}')) then
  begin
    Exec(ExpandConstant('{#Launch}'), 'stop', '', SW_HIDE, ewWaitUntilTerminated, Code);
    Old := ExpandConstant('{localappdata}\{#AppName}');
    if CompareText(Old, TempoHome) <> 0 then
    begin
      SetEnvironmentVariable('TEMPO_LAUNCH_HOME', Old);
      Exec(ExpandConstant('{#Launch}'), 'stop', '', SW_HIDE, ewWaitUntilTerminated, Code);
      SetEnvironmentVariable('TEMPO_LAUNCH_HOME', '');
    end;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    { what the programs Setup starts (the vault, the first launch) should already see }
    if (VaultPage <> nil) and WizardIsComponentSelected('vault') then
      SetEnvironmentVariable('TEMPO_VAULT', VaultDir(''));
    if UserEnvMissing('TEMPO_AGENT_DATA') then
      SetEnvironmentVariable('TEMPO_AGENT_DATA', TempoAgentData(''));
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
var
  S: String;
begin
  { the Ollama box starts ticked only when no Ollama is on this computer; chosen once, so the
    person's own change survives going back and forward }
  { a silent install does exactly what /TASKS says: nothing is ticked for it }
  if (CurPageID = wpSelectTasks) and not OllamaTaskDecided and not WizardSilent then
  begin
    OllamaTaskDecided := True;
    if not OllamaPresent then
      WizardSelectTasks('getollama');
  end;
  if CurPageID = wpFinished then
  begin
    S := '';
    if (VaultPage <> nil) and WizardIsComponentSelected('vault') then
      S := 'Your vault is in ' + VaultDir('') + '.' + #13#10#13#10;
    if WizardIsTaskSelected('getollama') and WizardIsComponentSelected('agent') then
      S := S + 'Ollama is set up: Perennos Tempo downloaded its own copy, checked it against a pinned SHA-256 before running it, and uses it for models. If anything went wrong, run "tempo-launch setup --apply" later. The first time you open Perennos Tempo, it shows which models fit.' + #13#10#13#10
    else if OllamaPresent then
      S := S + 'Ollama was found on this computer. The first time you open Perennos Tempo, it shows which models fit.' + #13#10#13#10
    else
      S := S + 'Ollama was not found on this computer. Perennos Tempo uses it to run models. Run "tempo-launch setup --apply", or get it from https://ollama.com/download. The first time you open Perennos Tempo, it shows which models fit this computer.' + #13#10#13#10;
    S := S + 'Open Perennos Tempo any time from the Start menu. Everything stays on this computer.';
    WizardForm.FinishedLabel.Caption := S;
    WizardForm.FinishedLabel.AdjustHeight;
    WizardForm.RunList.Top := WizardForm.FinishedLabel.Top + WizardForm.FinishedLabel.Height + ScaleY(16);
  end;
end;

{ ---- uninstall }

function InitializeUninstall: Boolean;
begin
  KeptVault := UserEnv('TEMPO_VAULT');
  if (KeptVault = '') and DirExists(ExpandConstant('{userdocs}\Tempo Vault')) then
    KeptVault := ExpandConstant('{userdocs}\Tempo Vault');
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  S: String;
begin
  if CurUninstallStep = usUninstall then
    RemoveFromPath(ExpandConstant('{app}\bin'));
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
  begin
    S := 'Perennos Tempo has been removed.' + #13#10#13#10 + 'These are still on this computer, as they are yours:' + #13#10;
    if KeptVault <> '' then
      S := S + '  your notes: ' + KeptVault + #13#10;
    S := S + '  Tempo''s memory and logs: ' + TempoHome + #13#10#13#10 +
      'Delete them yourself if you no longer want them.';
    MsgBox(S, mbInformation, MB_OK);
  end;
end;
