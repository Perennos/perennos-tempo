<#
.SYNOPSIS
  Makes THIS machine trust packages signed with your Perennos certificate (run once per machine, as administrator).

.DESCRIPTION
  Imports the public certificate (the .cer from new-perennos-cert.ps1) into Cert:\LocalMachine\TrustedPeople, which is
  the store Windows checks before it installs a sideloaded MSIX. It changes the machine's trust settings, so it needs an
  administrator window: right-click PowerShell, "Run as administrator".

  Only a public certificate is accepted (a .cer). A file that holds a private key (.pfx) is refused.
  Trusting this certificate means: any package signed with the matching private key will install on this machine without
  a warning. That is the point, and also why the private key must stay on the one machine that made it.

  -Remove takes the certificate out again.

.EXAMPLE
  .\trust-perennos-cert.ps1 -Cer D:\perennos-tempo-signing.cer
#>
[CmdletBinding()]
param(
  [string]$Cer = (Join-Path $PSScriptRoot "..\build\perennos-tempo-signing.cer"),
  [switch]$Remove
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2

$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = (New-Object Security.Principal.WindowsPrincipal $id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
  throw "This needs administrator rights, because it changes what this whole machine trusts. Close this window, right-click PowerShell (or Terminal), choose 'Run as administrator', and run the same command again."
}
if (-not (Test-Path $Cer)) { throw "No certificate file at $Cer. Pass the .cer that new-perennos-cert.ps1 made: -Cer <path>" }
$Cer = (Resolve-Path $Cer).Path
if ([IO.Path]::GetExtension($Cer).ToLower() -ne ".cer") { throw "Only a public .cer file is accepted (got $Cer). Never copy a .pfx or a private key between machines." }

$cert = New-Object Security.Cryptography.X509Certificates.X509Certificate2 $Cer
if ($cert.HasPrivateKey) { throw "That file holds a private key. Refusing. Use the public .cer from new-perennos-cert.ps1." }
if ($cert.Subject -ne "CN=Perennos") { throw "The certificate subject is '$($cert.Subject)', not CN=Perennos. Refusing: this script only trusts the Perennos certificate." }
if ($cert.NotAfter -lt (Get-Date)) { throw "That certificate expired on $($cert.NotAfter.ToString('yyyy-MM-dd')). Make a new one with new-perennos-cert.ps1 -Force." }

if ($Remove) {
  $found = @(Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object { $_.Thumbprint -eq $cert.Thumbprint })
  if ($found.Count -eq 0) { Write-Host "It was not trusted here: nothing to remove."; return }
  $found | Remove-Item
  Write-Host "Removed. Packages signed with it no longer install on this machine."
  return
}

Import-Certificate -FilePath $Cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null
Write-Host "This machine now trusts packages signed with the Perennos certificate." -ForegroundColor White
Write-Host "Thumbprint: $($cert.Thumbprint)"
Write-Host "Check that it is the same thumbprint new-perennos-cert.ps1 printed on the signing machine."
Write-Host "Install a signed package with:  Add-AppxPackage <path>\PerennosTempo-<version>-x64-signed.msix"
