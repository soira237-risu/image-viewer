<#
.SYNOPSIS
Reports the size of the build cache (target/) and the free space on its drive; with -Clean,
deletes the cache when it is over the limits.

.DESCRIPTION
Cargo never removes old build output: every change of code, features or package selection adds
new artifacts beside the old ones, so target/ only grows (it reached 51 GB here and filled the
disk). Run this before and after long build sessions.

Exit codes: 0 = within limits (or cleaned), 1 = over a limit and -Clean not given.

.EXAMPLE
pwsh scripts/cache-check.ps1            # report only
pwsh scripts/cache-check.ps1 -Clean     # report, and run `cargo clean` when over a limit
#>
param(
    # target/ larger than this is cleaned
    [double]$MaxTargetGB = 20,
    # less free space than this on the drive is cleaned too
    [double]$MinFreeGB = 30,
    [switch]$Clean
)

$root = Split-Path $PSScriptRoot -Parent
$target = Join-Path $root 'target'

function Get-SizeGB([string]$Path) {
    if (-not (Test-Path $Path)) { return 0.0 }
    $sum = (Get-ChildItem $Path -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
    return [double]$sum / 1GB
}

$debug = Get-SizeGB (Join-Path $target 'debug')
$release = Get-SizeGB (Join-Path $target 'release')
$total = Get-SizeGB $target
$drive = (Get-Item $root).PSDrive
$free = [double]$drive.Free / 1GB

"target: {0:N1} GB (debug {1:N1} GB, release {2:N1} GB) | {3}: {4:N1} GB free" -f $total, $debug, $release, $drive.Name, $free

$reasons = @()
if ($total -gt $MaxTargetGB) { $reasons += "target over $MaxTargetGB GB" }
if ($free -lt $MinFreeGB) { $reasons += "less than $MinFreeGB GB free" }
if ($reasons.Count -eq 0) { "OK"; exit 0 }

"over the limit: " + ($reasons -join ', ')
if (-not $Clean) { "run with -Clean to delete target/ (it is rebuilt on the next build)"; exit 1 }

$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
Push-Location $root
try { & $cargo clean } finally { Pop-Location }
"cleaned; {0}: {1:N1} GB free" -f $drive.Name, ([double](Get-PSDrive $drive.Name).Free / 1GB)
exit 0
