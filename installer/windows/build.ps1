<#
.SYNOPSIS
  Build the Perennos Tempo installer on Windows.

.DESCRIPTION
  Compiles every product (x86_64-pc-windows-msvc by default), draws the art, writes the third-party
  notices, then runs Inno Setup's compiler. The result is build\out\PerennosTempo-Setup-<version>-x64.exe
  and its .sha256. The same steps as build.sh; README.md says what each one needs.

.EXAMPLE
  .\installer\windows\build.ps1

.EXAMPLE
  .\installer\windows\build.ps1 -Iscc "D:\Tools\Inno Setup 6\ISCC.exe" -Target x86_64-pc-windows-gnu

.EXAMPLE
  # signed (needs a code-signing certificate in the store)
  .\installer\windows\build.ps1 -SignTool 'signtool.exe sign /a /fd sha256 /tr http://timestamp.digicert.com /td sha256 $f'
#>
param(
  [string]$Iscc = "",
  [string]$Target = "x86_64-pc-windows-msvc",
  [string]$SignTool = "",
  [string]$LicenseFile = "",
  # the apps to include (the launcher and the agent always are); the source-available edition:
  # -Apps vault,studio,room (without the paid Field and VECTIS's tempo command)
  [string[]]$Apps = @("vault", "studio", "room", "field", "cli")
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2

$Here = $PSScriptRoot
$Root = (Resolve-Path (Join-Path $Here "..\..")).Path
$Build = if ($env:TEMPO_BUILD_DIR) { $env:TEMPO_BUILD_DIR } else { Join-Path $Here "build" }
$Launcher = Join-Path $Root "crates\perennos-tempo-launcher"
if (-not (Test-Path $Launcher)) { $Launcher = Join-Path $Root "perennos-tempo-launcher" }
$Version = $env:TEMPO_VERSION
if (-not $Version) {
  $m = Select-String -Path (Join-Path $Launcher "Cargo.toml") -Pattern '^version = "(.*)"' | Select-Object -First 1
  $Version = $m.Matches[0].Groups[1].Value
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "the version must be numbers like 0.1.0 (got '$Version')" }

# Where each product's crate is: crates\NAME in the private checkout, NAME at the top of the public
# repository, unless TEMPO_SRC_<NAME> says otherwise.
function Src([string]$Name, [string]$Dir) {
  $v = [Environment]::GetEnvironmentVariable("TEMPO_SRC_$Name")
  if ($v) { return $v }
  $c = Join-Path $Root "crates\$Dir"
  if (Test-Path $c) { $c } else { Join-Path $Root $Dir }
}
$Agent = Src "AGENT" "perennos-tempo-local-agent"
$Vault = Src "VAULT" "perennos-tempo-vault"
$Studio = Src "STUDIO" "perennos-tempo-studio"
$Room = Src "ROOM" "perennos-tempo-room"
$Field = Src "FIELD" "perennos-tempo-dashboard"
$Cli = Src "CLI" "perennos-tempo-cli"

function Say([string]$Text) { Write-Host $Text -ForegroundColor White }

$Apps = @($Apps | ForEach-Object { $_ -split '[,\s]+' } | Where-Object { $_ })
foreach ($a in $Apps) {
  if (@("vault", "studio", "room", "field", "cli") -notcontains $a) { throw "-Apps: '$a' is not an app (vault, studio, room, field, cli)" }
}
# The full edition has the paid parts (the Field, VECTIS's tempo command); without them it is
# the source-available edition, and its file name says so.
if (($Apps -contains "field") -or ($Apps -contains "cli")) { $Edition = "full"; $Suffix = "" }
else { $Edition = "source-available"; $Suffix = "-source-available" }
$Legal = Join-Path $Here "legal"
$shield = Join-Path $Legal "PolyForm-Shield-1.0.0.txt"
if ((Get-FileHash -Algorithm SHA256 $shield).Hash.ToLower() -ne "56328093d57c87dcf2811ddcc824caf2723a07ffc332e6fbe4f9f108a2893a91") {
  throw "legal\PolyForm-Shield-1.0.0.txt is not the PolyForm Shield 1.0.0 text of the public repo"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw "cargo is needed (https://rustup.rs)" }
# Python 3: the py launcher first (the "python" on a fresh Windows can be the Store's placeholder)
if (Get-Command py -ErrorAction SilentlyContinue) { $PyExe = "py"; $PyArgs = @("-3") }
elseif (Get-Command python -ErrorAction SilentlyContinue) { $PyExe = "python"; $PyArgs = @() }
else { throw "Python 3 is needed for the art and the notices (https://www.python.org)" }
function Run-Py {
  & $PyExe @PyArgs @args
  if ($LASTEXITCODE -ne 0) { throw "python $($args[0]) failed" }
}

if (-not $Iscc) {
  foreach ($c in @("${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe", "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe")) {
    if (Test-Path $c) { $Iscc = $c; break }
  }
}
if (-not $Iscc -or -not (Test-Path $Iscc)) { throw "Inno Setup 6.4 or later is needed: https://jrsoftware.org/isdl.php (or pass -Iscc)" }

# rc.exe gives tempo-launch.exe its icon and version details; the Windows SDK has it
if ($Target -like "*-msvc" -and -not (Get-Command rc.exe -ErrorAction SilentlyContinue)) {
  $kits = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
  $rc = Get-ChildItem -Path $kits -Filter rc.exe -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -like "*\x64\rc.exe" } | Sort-Object FullName -Descending | Select-Object -First 1
  if ($rc) { $env:PATH = "$($rc.DirectoryName);$env:PATH" }
}

# a fresh stage for every build, so nothing from an earlier one can slip in
$Stage = Join-Path $Build ("stage\$Version-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
$Art = Join-Path $Build "art"
$Out = Join-Path $Build "out"
New-Item -ItemType Directory -Force -Path (Join-Path $Stage "legal"), (Join-Path $Stage "examples"), $Art, $Out | Out-Null

Say "1/5 the art"
Run-Py (Join-Path $Here "art\make_art.py") $Art
$env:TEMPO_LAUNCH_ICON = Join-Path $Art "tempo.ico"

Say "2/5 the programs ($Target)"
$script:Built = @()
function Build-One([string]$Name, [string]$Dir, [string]$Bin, [bool]$Required) {
  if (-not (Test-Path (Join-Path $Dir "Cargo.toml"))) {
    if ($Required) { throw "${Name}: no crate at $Dir" }
    Write-Host "  ${Name}: no crate at $Dir, so not in this installer"
    return
  }
  Write-Host "  ${Name}: $Dir"
  $env:CARGO_TARGET_DIR = Join-Path $Build "target\$Name"
  & cargo build --quiet --release --locked --target $Target --manifest-path (Join-Path $Dir "Cargo.toml")
  if ($LASTEXITCODE -ne 0) { throw "$Name did not build" }
  Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$Target\release\$Bin.exe") $Stage
  $script:Built += $Dir
}
Build-One "launcher" $Launcher "tempo-launch" $true
Build-One "agent" $Agent "tempo-local-agent" $true
foreach ($app in @(@("vault", $Vault, "tempo-vault"), @("studio", $Studio, "tempo-studio"), @("room", $Room, "tempo-room"),
                   @("field", $Field, "tempo-dashboard"), @("cli", $Cli, "tempo"))) {
  if ($Apps -contains $app[0]) { Build-One $app[0] $app[1] $app[2] $false }
  else { Write-Host "  $($app[0]): not chosen (-Apps)" }
}
Remove-Item Env:\CARGO_TARGET_DIR

Say "3/5 licenses, notices and examples ($Edition edition)"
$notices = Join-Path $Stage "legal\THIRD-PARTY-NOTICES.txt"
Run-Py (Join-Path $Here "notices.py") $notices $Target @script:Built
$notice = Join-Path $Legal "PolyForm-Shield-notice.txt"
Copy-Item $shield (Join-Path $Stage "legal\LICENSE-PolyForm-Shield-1.0.0.txt")
Copy-Item $notice (Join-Path $Stage "legal\NOTICE.txt")
# the license page: what is under PolyForm Shield, what is proprietary, then the third-party notices
$openNames = "Tempo Agent"
foreach ($a in @(@("vault", "Tempo Vault"), @("studio", "Tempo Studio"), @("room", "Tempo Room"))) {
  if (($Apps -contains $a[0]) -and (Test-Path (Join-Path $Stage "tempo-$($a[0]).exe"))) { $openNames += ", $($a[1])" }
}
$openNames += " and tempo-launch"
$Page = Join-Path $Stage "license-page.rtf"
if ($Edition -eq "full") {
  $closed = Join-Path $Legal "Tempo-Field-and-VECTIS.txt"
  Copy-Item $closed (Join-Path $Stage "legal\LICENSE-Tempo-Field-and-VECTIS.txt")
  Run-Py (Join-Path $Here "license_page.py") $Page `
    --intro "Perennos Tempo is made of parts under different terms. Please read them; accepting below accepts all three." `
    --part "1. ${openNames}: PolyForm Shield License 1.0.0" $notice $shield `
    --part "2. Tempo Field and the tempo command, including VECTIS: proprietary" $closed `
    --part "3. Third-party notices" $notices
} else {
  Run-Py (Join-Path $Here "license_page.py") $Page `
    --part "1. Perennos Tempo, source-available edition (${openNames}): PolyForm Shield License 1.0.0" $notice $shield `
    --part "2. Third-party notices" $notices
}
foreach ($pair in @(@("agent", $Agent), @("studio", $Studio), @("room", $Room))) {
  $f = Join-Path $pair[1] "config.example.json"
  if (Test-Path $f) {
    $text = (Get-Content -Raw $f) -replace '"owner_name":\s*"[^"]*"', '"owner_name": "Your name"'
    [IO.File]::WriteAllText((Join-Path $Stage "examples\$($pair[0])-config.example.json"), $text)
  }
}

Say "4/5 the installer (Inno Setup)"
$isccArgs = @("/Q", "/DAppVersion=$Version", "/DStage=$Stage", "/DArtDir=$Art", "/DOutDir=$Out")
if ($SignTool) { $isccArgs += @("/Stempo=$SignTool", "/DSignTool=tempo") }
# -LicenseFile: another license page in place of the one made above
if ($LicenseFile) { $isccArgs += "/DLicenseFile=$((Resolve-Path $LicenseFile).Path)" } else { $isccArgs += "/DLicenseFile=$Page" }
if ($Edition -ne "full") { $isccArgs += "/DEdition=$Edition" }
& $Iscc @isccArgs (Join-Path $Here "PerennosTempo.iss")
if ($LASTEXITCODE -ne 0) { throw "Inno Setup could not make the installer" }

Say "5/5 the checksum"
$Name = "PerennosTempo-Setup-$Version-x64$Suffix.exe"
$Setup = Join-Path $Out $Name
$hash = (Get-FileHash -Algorithm SHA256 $Setup).Hash.ToLower()
[IO.File]::WriteAllText("$Setup.sha256", "$hash  $Name`n")
Write-Host "$hash  $Name"
Write-Host "$Edition edition, built from: $($script:Built -join ', ')"
