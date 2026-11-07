<#
.SYNOPSIS
  Makes the free "Perennos" code-signing certificate for signing Perennos Tempo packages for YOUR OWN machines.

.DESCRIPTION
  Run this ONCE, on the one machine that will sign packages (not on every machine).

  - It makes a self-signed certificate with the subject CN=Perennos, valid 3 years, in your own certificate store
    (Cert:\CurrentUser\My).
  - The private key is made NON-EXPORTABLE: it can be used on this machine to sign, but it can never be copied out of
    it, not even by you. If this machine is reinstalled the key is gone; you simply make a new certificate.
  - Only the PUBLIC half is exported, to perennos-tempo-signing.cer. That file is safe to copy anywhere (it holds no
    secret). Each of your other machines runs trust-perennos-cert.ps1 with it, once, as administrator.

  This costs nothing. It is NOT a certificate that strangers' computers trust: it only works where you import it.
  Strangers get Perennos Tempo from the Microsoft Store instead (the Store signs it).

.EXAMPLE
  .\new-perennos-cert.ps1
#>
[CmdletBinding()]
param(
  [int]$Years = 3,
  # where the public .cer goes
  [string]$OutDir = (Join-Path $PSScriptRoot "..\build"),
  # make a new one even if a valid CN=Perennos certificate is already here
  [switch]$Force
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2

if (-not (Get-Command New-SelfSignedCertificate -ErrorAction SilentlyContinue)) {
  throw "New-SelfSignedCertificate is not available. Run this in Windows PowerShell 5.1 or PowerShell 7 on Windows 10 or 11."
}
if ($Years -lt 1 -or $Years -gt 5) { throw "-Years must be 1 to 5" }

$have = @(Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq "CN=Perennos" -and $_.HasPrivateKey -and $_.NotAfter -gt (Get-Date).AddDays(60) })
if ($have.Count -gt 0 -and -not $Force) {
  $cert = $have | Sort-Object NotAfter -Descending | Select-Object -First 1
  Write-Host "A valid Perennos certificate is already here (valid until $($cert.NotAfter.ToString('yyyy-MM-dd'))). Using it. Pass -Force to make another." -ForegroundColor White
} else {
  # a code-signing certificate (extended key usage 1.3.6.1.5.5.7.3.3), not a certificate authority, key never leaves this machine
  $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=Perennos" `
    -KeyUsage DigitalSignature -KeyExportPolicy NonExportable `
    -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 `
    -NotAfter (Get-Date).AddYears($Years) `
    -FriendlyName "Perennos Tempo package signing (self-signed, own machines)" `
    -CertStoreLocation Cert:\CurrentUser\My
  Write-Host "Made the certificate CN=Perennos (valid until $($cert.NotAfter.ToString('yyyy-MM-dd')))." -ForegroundColor White
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$cer = Join-Path (Resolve-Path $OutDir).Path "perennos-tempo-signing.cer"
# the PUBLIC certificate only: Export-Certificate never writes a private key
Export-Certificate -Cert $cert -FilePath $cer -Type CERT | Out-Null

Write-Host ""
Write-Host "Public certificate (safe to copy): $cer"
Write-Host "Thumbprint: $($cert.Thumbprint)"
Write-Host ""
Write-Host "Next:"
Write-Host "  1. On THIS machine, as administrator:    .\trust-perennos-cert.ps1 -Cer `"$cer`""
Write-Host "  2. Build and sign:                       .\build-msix.ps1 -SignWithMyCertificate"
Write-Host "  3. On each other machine, as administrator, with the .cer copied there: .\trust-perennos-cert.ps1 -Cer <path to the .cer>"
Write-Host "     then install the SIGNED .msix that step 2 made (copy that to the other machines, never the key)."
Write-Host ""
Write-Host "The private key stays in this machine's certificate store and cannot be exported. Sign here; copy the signed packages."
