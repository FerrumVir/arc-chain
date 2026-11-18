# Run only against an unsigned MSI built from the candidate checkout in CI.
# A responsive visible HWND is observable on the hosted runner; foreground
# focus, physical shortcut clicks and installer update still need manual tests.
param(
    [Parameter(Mandatory=$true)][string] $MsiPath,
    [Parameter(Mandatory=$true)][string] $EvidenceDirectory
)
$ErrorActionPreference = 'Stop'
$msi = (Resolve-Path $MsiPath).Path
New-Item -ItemType Directory -Force $EvidenceDirectory | Out-Null
$evidence = (Resolve-Path $EvidenceDirectory).Path
$testProfile = Join-Path $evidence 'profile'
$target = Join-Path $evidence 'extracted'
New-Item -ItemType Directory -Force $testProfile, $target | Out-Null
$extract = Start-Process msiexec.exe -ArgumentList @('/a', ('"{0}"' -f $msi), '/qn',
    ('TARGETDIR="{0}"' -f $target), '/l*v', ('"{0}"' -f (Join-Path $evidence 'msi.log'))) -Wait -PassThru
if ($extract.ExitCode -ne 0) { throw "candidate MSI extraction failed: $($extract.ExitCode)" }
$apps = @(Get-ChildItem $target -Recurse -File -Filter arc-desktop.exe)
if ($apps.Count -ne 1) { throw 'expected exactly one candidate desktop' }
$exe = $apps[0].FullName
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class ArcInstanceWindow {
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
}
'@
$prior = @{}
$values = @{
    USERPROFILE = $testProfile; HOME = $testProfile
    APPDATA = (Join-Path $testProfile 'AppData\Roaming')
    LOCALAPPDATA = (Join-Path $testProfile 'AppData\Local')
    TEMP = (Join-Path $testProfile 'Temp'); TMP = (Join-Path $testProfile 'Temp')
}
foreach ($key in $values.Keys) {
    $prior[$key] = [Environment]::GetEnvironmentVariable($key)
    New-Item -ItemType Directory -Force $values[$key] | Out-Null
    [Environment]::SetEnvironmentVariable($key, $values[$key])
}
$children = [Collections.Generic.List[Diagnostics.Process]]::new()
function Start-Copy([string] $Name, [string[]] $Arguments = @()) {
    $options = @{
        FilePath = $exe; PassThru = $true
        RedirectStandardOutput = (Join-Path $evidence "$Name.stdout")
        RedirectStandardError = (Join-Path $evidence "$Name.stderr")
    }
    if ($Arguments.Count -gt 0) { $options.ArgumentList = $Arguments }
    $child = Start-Process @options
    $children.Add($child)
    return $child
}
function Assert-Secondary([string] $Name, [string[]] $Arguments = @()) {
    $child = Start-Copy $Name $Arguments
    if (-not $child.WaitForExit(5000)) { throw "$Name did not exit within five seconds" }
    if ($child.ExitCode -ne 0) { throw "$Name failed: $($child.ExitCode)" }
}
function Assert-OneOwner {
    $desktop.Refresh()
    $copies = @(Get-Process -Name arc-desktop -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })
    if ($desktop.HasExited -or $copies.Count -ne 1 -or $copies[0].Id -ne $desktop.Id) {
        throw 'second launch did not preserve exactly the original process'
    }
}
function Assert-Visible([IntPtr] $Window) {
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $desktop.Refresh()
        if ($desktop.HasExited) { throw 'owner exited' }
        if ($desktop.Responding -and [ArcInstanceWindow]::IsWindowVisible($Window) -and
            -not [ArcInstanceWindow]::IsIconic($Window)) { return }
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'original window did not become responsive, visible and un-minimized'
}
try {
    $desktop = Start-Copy 'owner'
    $deadline = [DateTime]::UtcNow.AddSeconds(60)
    do {
        Start-Sleep -Milliseconds 100
        $desktop.Refresh()
        if ($desktop.HasExited) { throw "owner exited during startup: $($desktop.ExitCode)" }
        $window = $desktop.MainWindowHandle
    } while ($window.ToInt64() -eq 0 -and [DateTime]::UtcNow -lt $deadline)
    if ($window.ToInt64() -eq 0) { throw 'no candidate desktop window' }
    Assert-Visible $window
    [void][ArcInstanceWindow]::ShowWindow($window, 6) # Minimize.
    Assert-Secondary 'second'
    Assert-OneOwner
    Assert-Visible $window
    [void][ArcInstanceWindow]::ShowWindow($window, 0) # Hide.
    Assert-Secondary 'login' @('--minimized')
    Assert-OneOwner
    # Allow queued main-thread work to run before declaring it stayed hidden.
    Start-Sleep -Milliseconds 750
    if ([ArcInstanceWindow]::IsWindowVisible($window)) { throw 'login copy raised the window' }
    Assert-Secondary 'reopen'
    Assert-OneOwner
    Assert-Visible $window
    foreach ($log in Get-ChildItem $evidence -File | Where-Object { $_.Extension -in '.stdout', '.stderr' }) {
        if ((Get-Content $log.FullName -Raw) -match 'another ARC desktop') {
            throw "lifecycle contention in $($log.Name)"
        }
    }
    [ordered]@{
        source_commit = (git rev-parse HEAD)
        package_sha256 = (Get-FileHash $msi -Algorithm SHA256).Hash
        binary_sha256 = (Get-FileHash $exe -Algorithm SHA256).Hash
        owner_pid = $desktop.Id
        window_handle = $window.ToInt64()
        second_exit_zero = $true; login_stayed_hidden = $true; reopen_visible = $true
        physical_focus_verified = $false; installer_update_verified = $false
    } | ConvertTo-Json | Set-Content -Encoding utf8NoBOM (Join-Path $evidence 'result.json')
} finally {
    # Only children recorded by this script; never kill processes by name.
    foreach ($child in $children) {
        $child.Refresh()
        if (-not $child.HasExited) {
            Stop-Process -Id $child.Id -Force
            [void]$child.WaitForExit(5000)
        }
    }
    foreach ($key in $prior.Keys) { [Environment]::SetEnvironmentVariable($key, $prior[$key]) }
}
