# Applies only the displayed, hash-approved per-program complement block list. No elevation or global policy changes.
param([switch]$Rollback)
$ErrorActionPreference = 'Stop'
$plan = [Console]::In.ReadToEnd() | ConvertFrom-Json
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Administrator rights are required after owner approval.' }
if ($plan.plan_sha256 -notmatch '^[a-f0-9]{64}$' -or $plan.enforcement -ne 'owner-approval-required') { throw 'Invalid approved plan.' }
$prefix = if ($Rollback) { 'REMOVE ' } else { 'APPLY ' }
if ($plan.owner_confirmation -ne ($prefix + $plan.plan_sha256.Substring(0,12))) { throw 'Exact owner approval is missing.' }
$group = 'PerennosTempo-Egress-' + $plan.plan_sha256.Substring(0,12)
$created = @()
$mutex = New-Object Threading.Mutex($false, ('Local\' + $group))
if (-not $mutex.WaitOne(0)) { $mutex.Dispose(); throw 'This exact policy is already being changed.' }
try {
  if (-not $Rollback -and @((Get-NetFirewallProfile) | Where-Object { -not $_.Enabled }).Count -gt 0) { throw 'A Windows Firewall profile is disabled. No global policy was changed.' }
  for ($index=0; $index -lt $plan.programs.Count; $index++) {
    $program = [IO.Path]::GetFullPath($plan.programs[$index])
    $name = $group + '-' + $index
    if ($Rollback) {
      $rule = Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue
      if ($rule) {
        if ($rule.Group -ne $group -or ($rule | Get-NetFirewallApplicationFilter).Program -ine $program) { throw 'Rule ownership does not match; nothing unrelated was removed.' }
        $rule | Remove-NetFirewallRule
      }
      continue
    }
    if (-not (Test-Path -LiteralPath $program -PathType Leaf)) { throw 'Executable is missing.' }
    $actual = (Get-FileHash -LiteralPath $program -Algorithm SHA256).Hash.ToLowerInvariant()
    $expected = $plan.executable_sha256.PSObject.Properties[$program].Value
    if ($actual -ne $expected) { throw 'Executable changed since approval.' }
    if (Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue) { throw 'This rule already exists; it was not replaced.' }
    $created += $name
    New-NetFirewallRule -Name $name -DisplayName $name -Group $group -Direction Outbound -Action Block -Enabled True -Profile Any -Program $program -RemoteAddress @($plan.blocked_networks) | Out-Null
  }
  $status = if ($Rollback) { 'removed' } else { 'applied' }
  @{status=$status; plan_sha256=$plan.plan_sha256; group=$group; rules=$created} | ConvertTo-Json -Compress
} catch {
  foreach ($name in $created) {
    $rule = Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue
    if ($rule -and $rule.Group -eq $group) { $rule | Remove-NetFirewallRule -ErrorAction Continue }
  }
  throw
} finally {
  $mutex.ReleaseMutex(); $mutex.Dispose()
}
