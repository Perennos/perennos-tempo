<#
.SYNOPSIS
  Build the Perennos Tempo MSIX package on Windows (free: needs only the free Windows SDK).

.DESCRIPTION
  Compiles the programs (or takes ones you already built), lays them out the way AppxManifest.xml expects,
  fills the manifest, runs makeappx, and then does ONE of two things:

    -Store                  leaves the package UNSIGNED, for upload to Partner Center. The Microsoft Store signs it itself
                            after certification. Also writes an .msixupload (a zip of the .msix) next to it.
    -SignWithMyCertificate  signs it with the free self-signed "Perennos" certificate that new-perennos-cert.ps1 made
                            on this machine, for installing on your own machines (see trust-perennos-cert.ps1).

  The edition is chosen like build.ps1 chooses it: -Apps vault,studio,room,field,cli is the full edition (the default);
  -Apps vault,studio,room is the source-available one (no Field and no `tempo` command).
  README.md (next to this file) walks through both paths.

.EXAMPLE
  # for the Store: the three values come from Partner Center > Product identity
  .\build-msix.ps1 -Store -IdentityName "12345Perennos.PerennosTempo" -Publisher "CN=00000000-0000-0000-0000-000000000000" -PublisherDisplayName "Perennos" -Apps vault,studio,room

.EXAMPLE
  # for my own machines (full edition), signed with my certificate
  .\build-msix.ps1 -SignWithMyCertificate
#>
[CmdletBinding()]
param(
  [switch]$Store,
  [switch]$SignWithMyCertificate,
  # Store only: Partner Center > the app > Product management > Product identity
  [string]$IdentityName = "",
  [string]$Publisher = "",
  [string]$PublisherDisplayName = "Perennos",
  # four numbers; the default is the launcher's version with a 0 added (0.1.0 -> 0.1.0.0). The Store wants the last number to be 0.
  [string]$Version = "",
  [string]$Target = "x86_64-pc-windows-msvc",
  [string[]]$Apps = @("vault", "studio", "room", "field", "cli"),
  # a folder that already holds the built .exe files (tempo-launch.exe, tempo-local-agent.exe, ...): skips the cargo builds
  [string]$Prebuilt = "",
  # sign with this certificate (thumbprint) instead of looking up CN=Perennos in Cert:\CurrentUser\My
  [string]$CertificateThumbprint = "",
  [string]$TimestampUrl = "http://timestamp.digicert.com",
  # drop the write-virtualization settings and the unvirtualizedResources capability (only if the Store refuses them: README.md says what changes)
  [switch]$WithoutUnvirtualized,
  # a folder with makeappx.exe and signtool.exe, if the Windows SDK is somewhere unusual
  [string]$SdkBin = "",
  [string]$OutDir = ""
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2

function Say([string]$Text) { Write-Host $Text -ForegroundColor White }
function Warn([string]$Text) { Write-Host "WARNING: $Text" -ForegroundColor Yellow }

if ($Store -eq $SignWithMyCertificate) {
  throw "Choose exactly one: -Store (unsigned, for Partner Center) or -SignWithMyCertificate (signed with your own certificate, for your own machines)."
}

$Here = $PSScriptRoot
$WinDir = (Resolve-Path (Join-Path $Here "..")).Path
$Root = (Resolve-Path (Join-Path $Here "..\..\..")).Path
$Build = if ($env:TEMPO_BUILD_DIR) { $env:TEMPO_BUILD_DIR } else { Join-Path $WinDir "build" }
$Launcher = Join-Path $Root "crates\perennos-tempo-launcher"
if (-not $OutDir) { $OutDir = Join-Path $Build "msix-out" }

# ---- the Windows SDK tools (free): makeappx, signtool, and makepri if there
$SdkHelp = @"
The free Windows SDK is needed (it holds makeappx.exe and signtool.exe).
  Install it:   winget install Microsoft.WindowsSDK.10.0.26100      (check the exact id with: winget search Microsoft.WindowsSDK)
  or download:  https://developer.microsoft.com/windows/downloads/windows-sdk/
  In its installer, keep "Windows SDK Signing Tools for Desktop Apps" ticked.
Then run this script again. If the SDK is in an unusual place, pass -SdkBin "<folder that has makeappx.exe>".
"@
function Find-SdkTool([string]$Name, [bool]$Required) {
  if ($SdkBin) {
    $p = Join-Path $SdkBin $Name
    if (Test-Path $p) { return $p }
  }
  $c = Get-Command $Name -ErrorAction SilentlyContinue
  if ($c) { return $c.Source }
  $kits = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
  if (Test-Path $kits) {
    $f = Get-ChildItem -Path $kits -Filter $Name -Recurse -ErrorAction SilentlyContinue |
      Where-Object { $_.FullName -like "*\x64\$Name" } | Sort-Object FullName -Descending | Select-Object -First 1
    if ($f) { return $f.FullName }
  }
  if ($Required) { throw "$Name was not found.`n$SdkHelp" }
  return $null
}
$MakeAppx = Find-SdkTool "makeappx.exe" $true
$MakePri = Find-SdkTool "makepri.exe" $false
$SignTool = if ($SignWithMyCertificate) { Find-SdkTool "signtool.exe" $true } else { $null }

# ---- the apps in this edition
$Apps = @($Apps | ForEach-Object { $_ -split '[,\s]+' } | Where-Object { $_ })
foreach ($a in $Apps) {
  if (@("vault", "studio", "room", "field", "cli") -notcontains $a) { throw "-Apps: '$a' is not an app (vault, studio, room, field, cli)" }
}
if (($Apps -contains "field") -or ($Apps -contains "cli")) { $Edition = "full"; $Suffix = "" }
else { $Edition = "source-available"; $Suffix = "-source-available" }

# ---- identity
if (-not $Version) {
  $m = Select-String -Path (Join-Path $Launcher "Cargo.toml") -Pattern '^version = "(.*)"' | Select-Object -First 1
  $Version = $m.Matches[0].Groups[1].Value
}
if ($Version -match '^\d+\.\d+\.\d+$') { $Version = "$Version.0" }
if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') { throw "-Version must be four numbers like 1.0.0.0 (got '$Version')" }
if ($Store) {
  if (-not $IdentityName -or -not $Publisher) {
    throw "-Store needs -IdentityName and -Publisher exactly as Partner Center shows them (the app > Product management > Product identity: Package/Identity/Name and Package/Identity/Publisher). Reserve the name first; README.md, step 'Store'."
  }
  if ($Version -notmatch '\.0$') { Warn "The Store reserves the last number of the version for itself and wants it to be 0 (got $Version)." }
  if ($Version.StartsWith("0.")) { Warn "The version starts with 0. If Partner Center refuses it, build again with -Version 1.0.0.0 or higher." }
} else {
  if (-not $IdentityName) { $IdentityName = "Perennos.PerennosTempo" }
}
if ($IdentityName -notmatch '^[-A-Za-z0-9.]{3,50}$') { throw "the identity name may only hold letters, digits, '-' and '.' (3 to 50 characters): '$IdentityName'" }

# ---- the certificate (own machines): the package's Publisher must equal the certificate's Subject exactly
$Cert = $null
if ($SignWithMyCertificate) {
  $pool = @(Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.HasPrivateKey -and $_.NotAfter -gt (Get-Date) -and ($_.EnhancedKeyUsageList | Where-Object { $_.ObjectId -eq "1.3.6.1.5.5.7.3.3" }) })
  if ($CertificateThumbprint) { $pool = @($pool | Where-Object { $_.Thumbprint -eq $CertificateThumbprint.Replace(" ", "").ToUpper() }) }
  else { $pool = @($pool | Where-Object { $_.Subject -eq "CN=Perennos" }) }
  if ($pool.Count -eq 0) {
    throw "No usable code-signing certificate named CN=Perennos in Cert:\CurrentUser\My on this machine. Run new-perennos-cert.ps1 first (once, on the machine that signs). Signing certificates never leave the machine that made them."
  }
  $Cert = $pool | Sort-Object NotAfter -Descending | Select-Object -First 1
  $Publisher = $Cert.Subject
  Say "Signing with $($Cert.Subject) (thumbprint $($Cert.Thumbprint), valid until $($Cert.NotAfter.ToString('yyyy-MM-dd')))"
  if ($Cert.NotAfter -lt (Get-Date).AddDays(30)) { Warn "the certificate expires within 30 days: make a new one (new-perennos-cert.ps1 -Force) and trust it again on your machines" }
}

# ---- stage: a fresh folder for every build
$Stage = Join-Path $Build ("msix-stage\$Version-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
$Bin = Join-Path $Stage "bin"
New-Item -ItemType Directory -Force -Path $Bin, (Join-Path $Stage "legal"), $OutDir | Out-Null

Say "1/6 the programs"
$Built = @()
$py = $null
if ($Prebuilt) {
  foreach ($exe in @("tempo-launch", "tempo-local-agent")) {
    if (-not (Test-Path (Join-Path $Prebuilt "$exe.exe"))) { throw "-Prebuilt: $exe.exe is not in $Prebuilt" }
  }
  Get-ChildItem $Prebuilt -Filter *.exe | Copy-Item -Destination $Bin
} else {
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw "cargo is needed (https://rustup.rs)" }
  # the Tempo icon for tempo-launch.exe (the Windows SDK's rc.exe adds it); Python is optional here
  $Art = Join-Path $Build "art"
  New-Item -ItemType Directory -Force -Path $Art | Out-Null
  $py = if (Get-Command py -ErrorAction SilentlyContinue) { @("py", "-3") } elseif (Get-Command python -ErrorAction SilentlyContinue) { @("python") } else { $null }
  if ($py) {
    & $py[0] @($py | Select-Object -Skip 1) (Join-Path $WinDir "art\make_art.py") $Art | Out-Null
    if ($LASTEXITCODE -eq 0) { $env:TEMPO_LAUNCH_ICON = Join-Path $Art "tempo.ico" }
  } else { Warn "Python 3 not found: tempo-launch.exe is built without its icon" }
  if ($Target -like "*-msvc" -and -not (Get-Command rc.exe -ErrorAction SilentlyContinue)) {
    $rc = Get-ChildItem -Path "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Filter rc.exe -Recurse -ErrorAction SilentlyContinue |
      Where-Object { $_.FullName -like "*\x64\rc.exe" } | Sort-Object FullName -Descending | Select-Object -First 1
    if ($rc) { $env:PATH = "$($rc.DirectoryName);$env:PATH" }
  }
  function Src([string]$Name, [string]$Dir) {
    $v = [Environment]::GetEnvironmentVariable("TEMPO_SRC_$Name")
    if ($v) { $v } else { Join-Path $Root "crates\$Dir" }
  }
  function Build-One([string]$Name, [string]$Dir, [string]$BinName, [bool]$Required) {
    if (-not (Test-Path (Join-Path $Dir "Cargo.toml"))) {
      if ($Required) { throw "${Name}: no crate at $Dir" }
      Write-Host "  ${Name}: no crate at $Dir, so not in this package"
      return $false
    }
    Write-Host "  ${Name}: $Dir"
    $env:CARGO_TARGET_DIR = Join-Path $Build "target\$Name"
    & cargo build --quiet --release --locked --target $Target --manifest-path (Join-Path $Dir "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "$Name did not build" }
    Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$Target\release\$BinName.exe") $Bin
    $script:Built += $Dir
    return $true
  }
  [void](Build-One "launcher" $Launcher "tempo-launch" $true)
  [void](Build-One "agent" (Src "AGENT" "perennos-tempo-local-agent") "tempo-local-agent" $true)
  foreach ($app in @(@("vault", "perennos-tempo-vault", "tempo-vault"), @("studio", "perennos-tempo-studio", "tempo-studio"),
                     @("room", "perennos-tempo-room", "tempo-room"), @("field", "perennos-tempo-dashboard", "tempo-dashboard"),
                     @("cli", "perennos-tempo-cli", "tempo"))) {
    if ($Apps -contains $app[0]) { [void](Build-One $app[0] (Src ($app[0].ToUpper()) $app[1]) $app[2] $false) }
    else { Write-Host "  $($app[0]): not chosen (-Apps)" }
  }
  Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue
}
# the sign-in start: the same program under a name the launcher reads as "start" (a startup task cannot pass a parameter)
Copy-Item (Join-Path $Bin "tempo-launch.exe") (Join-Path $Bin "tempo-start.exe")
# what is really in the package decides what the manifest offers
$Present = @{}
foreach ($a in @("vault", "studio", "room", "cli")) {
  $exe = @{ vault = "tempo-vault"; studio = "tempo-studio"; room = "tempo-room"; cli = "tempo" }[$a]
  $Present[$a] = Test-Path (Join-Path $Bin "$exe.exe")
}

Say "2/6 legal texts"
$Legal = Join-Path $WinDir "legal"
Copy-Item (Join-Path $Legal "PolyForm-Shield-1.0.0.txt") (Join-Path $Stage "legal\LICENSE-PolyForm-Shield-1.0.0.txt")
Copy-Item (Join-Path $Legal "PolyForm-Shield-notice.txt") (Join-Path $Stage "legal\NOTICE.txt")
if ($Edition -eq "full") { Copy-Item (Join-Path $Legal "Tempo-Field-and-VECTIS.txt") (Join-Path $Stage "legal\LICENSE-Tempo-Field-and-VECTIS.txt") }
if ($py -and $Built.Count -gt 0) {
  & $py[0] @($py | Select-Object -Skip 1) (Join-Path $WinDir "notices.py") (Join-Path $Stage "legal\THIRD-PARTY-NOTICES.txt") $Target @Built | Out-Null
  if ($LASTEXITCODE -ne 0) { Warn "the third-party notices could not be written; the package has no THIRD-PARTY-NOTICES.txt" }
} else { Warn "third-party notices not written (they need Python and a build from source): add legal\THIRD-PARTY-NOTICES.txt before you submit" }

Say "3/6 the pictures and the manifest ($Edition edition)"
Copy-Item (Join-Path $Here "Assets") (Join-Path $Stage "Assets") -Recurse
$text = [IO.File]::ReadAllText((Join-Path $Here "AppxManifest.xml"), [Text.UTF8Encoding]::new($false))
$text = $text.Replace("@@IDENTITY_NAME@@", [Security.SecurityElement]::Escape($IdentityName)).Replace("@@PUBLISHER@@", [Security.SecurityElement]::Escape($Publisher))
$text = $text.Replace("@@VERSION@@", $Version).Replace("@@PUBLISHER_DISPLAY_NAME@@", [Security.SecurityElement]::Escape($PublisherDisplayName))
$keepVirt = -not $WithoutUnvirtualized
$text = [regex]::Replace($text, '[ \t]*<!--@(app:\w+|virt)-->\r?\n(.*?)[ \t]*<!--@/\1-->[ \t]*\r?\n', {
  param($m)
  $key = $m.Groups[1].Value
  $keep = if ($key -eq "virt") { $keepVirt } else { [bool]$Present[$key.Substring(4)] }
  if ($keep) { $m.Groups[2].Value } else { "" }
}, [Text.RegularExpressions.RegexOptions]::Singleline)
if ($text -match '@@') { throw "the manifest still has a placeholder" }
[xml]$check = $text   # well-formed, or this stops here with the line number
[IO.File]::WriteAllText((Join-Path $Stage "AppxManifest.xml"), $text, [Text.UTF8Encoding]::new($false))
foreach ($e in $check.GetElementsByTagName("Application")) {
  if (-not (Test-Path (Join-Path $Stage $e.GetAttribute("Executable")))) { throw "the manifest names $($e.GetAttribute('Executable')) but it is not in the package" }
}

Say "4/6 resource index (resources.pri)"
if ($MakePri) {
  $cfg = Join-Path $Build "priconfig.xml"
  & $MakePri createconfig /cf $cfg /dq en-US /o | Out-Null
  if ($LASTEXITCODE -eq 0) {
    & $MakePri new /pr $Stage /cf $cfg /mn (Join-Path $Stage "AppxManifest.xml") /of (Join-Path $Stage "resources.pri") /o | Out-Null
  }
  if ($LASTEXITCODE -ne 0) {
    Warn "makepri failed: the package is built without resources.pri (Windows then finds the pictures by their plain names; the sharpest sizes may not be picked)"
    Remove-Item (Join-Path $Stage "resources.pri") -ErrorAction SilentlyContinue
  }
} else { Warn "makepri.exe not found in the SDK: the package is built without resources.pri" }

Say "5/6 makeappx pack"
$mode = if ($Store) { "" } else { "-signed" }
$Msix = Join-Path $OutDir "PerennosTempo-$Version-x64$Suffix$mode.msix"
if (Test-Path $Msix) { Remove-Item $Msix }
& $MakeAppx pack /d $Stage /p $Msix /o
if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed (see its message above)" }

Say "6/6 $(if ($Store) { 'Store package (left unsigned)' } else { 'signing' })"
if ($Store) {
  # Partner Center takes the .msix itself or an .msixupload (a zip holding it); the Store signs it after certification
  $Upload = [IO.Path]::ChangeExtension($Msix, ".msixupload")
  $Zip = [IO.Path]::ChangeExtension($Msix, ".msixupload.zip")
  if (Test-Path $Upload) { Remove-Item $Upload }
  Compress-Archive -Path $Msix -DestinationPath $Zip -Force
  Move-Item $Zip $Upload
  Write-Host ""
  Write-Host "Upload either file to Partner Center (Packages page):"
  Write-Host "  $Msix   (unsigned on purpose: the Store signs it)"
  Write-Host "  $Upload"
  Write-Host "Do not install this package on a PC: it is unsigned. For your own machines run: build-msix.ps1 -SignWithMyCertificate"
} else {
  $signed = $false
  foreach ($url in (@($TimestampUrl) + @("http://timestamp.sectigo.com", "http://timestamp.globalsign.com/tsa/r6advanced1") | Select-Object -Unique)) {
    & $SignTool sign /fd SHA256 /sha1 $Cert.Thumbprint /tr $url /td SHA256 $Msix
    if ($LASTEXITCODE -eq 0) { $signed = $true; break }
    Warn "signing with timestamp server $url did not work; trying the next one"
  }
  if (-not $signed) {
    Remove-Item $Msix -ErrorAction SilentlyContinue
    throw "signtool could not sign (see its message above). Without a timestamp the signature would stop working when the certificate expires, so no half-signed package is left: $Msix was removed."
  }
  & $SignTool verify /pa $Msix
  if ($LASTEXITCODE -ne 0) {
    Warn "signtool verify /pa did not pass. That is expected until this machine trusts your certificate: run trust-perennos-cert.ps1 here once (as administrator), then check with: signtool verify /pa `"$Msix`""
  } else { Say "signature verified" }
  Write-Host ""
  Write-Host "Signed package: $Msix"
  Write-Host "Copy it to your other machines. Each needs trust-perennos-cert.ps1 run once (as administrator), then:  Add-AppxPackage `"<path>\$(Split-Path $Msix -Leaf)`""
}
$hash = (Get-FileHash -Algorithm SHA256 $Msix).Hash.ToLower()
[IO.File]::WriteAllText("$Msix.sha256", "$hash  $(Split-Path $Msix -Leaf)`n")
Write-Host "$hash  $(Split-Path $Msix -Leaf)"
Write-Host "$Edition edition, identity $IdentityName, version $Version"
