<#
.SYNOPSIS
Keeps the build cache (target/) from growing: deletes exactly the build output the current
workspace no longer uses, and reports size and free space.

.DESCRIPTION
Cargo never deletes old build output (rust-lang/cargo#13136). Each change of dependency
versions, features, profiles or the compiler adds a new unit folder
(target/<profile>/build/<package>/<hash>/) beside the old ones, so target/ only grows.

The sweep asks Cargo which units the standard commands use — `build`, `clippy` and `check`,
all with `--workspace --all-targets` (`cargo test` uses the units of `build --all-targets`) —
from their JSON messages, and deletes every other unit folder. Executables and libraries that
Cargo copies to target/<profile>/ are hard links into their unit folders; those folders are
found through the links. Incremental caches untouched for -StaleDays are deleted, and so is a
release build untouched for -StaleDays (release builds are only for measuring; use
-IncludeRelease to sweep it like debug instead).

Nothing is deleted when another Cargo or rustc is running, or when any of the commands fails
(the set of live units would be incomplete).

Exit codes: 0 = done and within limits, 1 = over a limit afterwards, 2 = skipped (busy or a
command failed).

.EXAMPLE
pwsh -File scripts/cache.ps1               # sweep stale output, report
pwsh -File scripts/cache.ps1 -DryRun       # show what would be deleted
pwsh -File scripts/cache.ps1 -Report       # sizes only, no cargo commands
pwsh -File scripts/cache.ps1 -Clean        # delete target/ entirely (also works when the disk is full)
pwsh -File scripts/cache.ps1 -Auto         # sweep only when target/ is over 15 GB or the drive is low
                                           # (run in the background at every Claude Code session start)
#>
param(
    [switch]$Report,
    [switch]$DryRun,
    [switch]$Clean,
    [switch]$IncludeRelease,
    # for the session-start hook: sweep only when target/ is over -AutoGB or the drive is low
    [switch]$Auto,
    [double]$AutoGB = 15,
    [int]$StaleDays = 7,
    [double]$MaxTargetGB = 20,
    [double]$MinFreeGB = 30
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$target = Join-Path $root 'target'
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
$nasm = Join-Path $env:LOCALAPPDATA 'bin\NASM'
if ((Test-Path $nasm) -and -not ($env:PATH -split ';' -contains $nasm)) { $env:PATH = "$nasm;$env:PATH" }

function Get-SizeGB([string]$Path) {
    if (-not (Test-Path $Path)) { return 0.0 }
    return [double](Get-ChildItem $Path -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1GB
}

function Show-Status([string]$Label) {
    $drive = (Get-Item $root).PSDrive
    $free = [double](Get-PSDrive $drive.Name).Free / 1GB
    $total = Get-SizeGB $target
    "{0}target {1:N1} GB (debug {2:N1}, release {3:N1}) | {4}: {5:N1} GB free" -f $Label, $total,
        (Get-SizeGB (Join-Path $target 'debug')), (Get-SizeGB (Join-Path $target 'release')), $drive.Name, $free
    return @{ Total = $total; Free = $free }
}

function Test-Limits($s) {
    $over = @()
    if ($s.Total -gt $MaxTargetGB) { $over += "target over $MaxTargetGB GB" }
    if ($s.Free -lt $MinFreeGB) { $over += "less than $MinFreeGB GB free" }
    if ($over) { Write-Host "over the limit: $($over -join ', ') — run with -Clean to delete target/ entirely"; return 1 }
    return 0
}

function Remove-Item-Safe([string]$Path) {
    if ($DryRun) { Write-Host "  would delete $($Path.Substring($target.Length + 1))"; return }
    Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction SilentlyContinue
}

if ($Clean) {
    Show-Status 'before: ' | Select-Object -First 1
    Push-Location $root
    try { & $cargo clean } finally { Pop-Location }
    $s = Show-Status 'after:  '; $s[0]; exit 0
}

$before = Show-Status 'before: '
$before[0]
if ($Auto -and $before[1].Total -le $AutoGB -and $before[1].Free -ge $MinFreeGB) { "auto: below $AutoGB GB, nothing to do"; exit 0 }
if ($Report -or -not (Test-Path $target)) { exit (Test-Limits $before[1]) }

$busy = Get-Process cargo, rustc, clippy-driver, rustdoc -ErrorAction SilentlyContinue
if ($busy) { "skipped: another build is running ($(($busy | ForEach-Object Name | Sort-Object -Unique) -join ', '))"; exit 2 }

# Unit folders and copied outputs that the given cargo commands use.
function Get-LiveSet([string]$Profile, $Commands) {
    $units = @{}; $files = @{}
    $profileDir = Join-Path $target $Profile
    foreach ($cmd in $Commands) {
        Push-Location $root
        try { $lines = & $cargo @cmd --locked --message-format=json --quiet 2>$null; $code = $LASTEXITCODE } finally { Pop-Location }
        if ($code -ne 0) { throw "cargo $($cmd -join ' ') failed (exit $code)" }
        foreach ($line in $lines) {
            if (-not $line.StartsWith('{"reason":"compiler-artifact"') -and -not $line.StartsWith('{"reason":"build-script-executed"')) { continue }
            $m = $line | ConvertFrom-Json
            $paths = @($m.filenames) + @($m.executable) + @($m.out_dir)
            foreach ($p in $paths) {
                if (-not $p -or -not $p.StartsWith($profileDir, [StringComparison]::OrdinalIgnoreCase)) { continue }
                $rel = $p.Substring($profileDir.Length + 1) -split '[\\/]'
                if ($rel.Length -ge 3 -and $rel[0] -eq 'build') { $units["$($rel[1])\$($rel[2])"] = $true }
                else { $files[(Resolve-Path -LiteralPath $p -ErrorAction SilentlyContinue).Path] = $true }
            }
        }
    }
    # copies in target/<profile>/ are hard links into their unit folders
    foreach ($f in @($files.Keys)) {
        if (-not $f) { continue }
        foreach ($link in (fsutil hardlink list $f 2>$null)) {
            $full = (Split-Path $root -Qualifier) + $link
            if ($full.StartsWith($profileDir, [StringComparison]::OrdinalIgnoreCase)) {
                $rel = $full.Substring($profileDir.Length + 1) -split '[\\/]'
                if ($rel.Length -ge 3 -and $rel[0] -eq 'build') { $units["$($rel[1])\$($rel[2])"] = $true }
            }
        }
    }
    return @{ Units = $units; Files = $files }
}

function Invoke-Sweep([string]$Profile, $Live) {
    $profileDir = Join-Path $target $Profile
    $removed = 0
    $build = Join-Path $profileDir 'build'
    if (Test-Path $build) {
        foreach ($pkg in Get-ChildItem $build -Directory) {
            foreach ($unit in Get-ChildItem $pkg.FullName -Directory) {
                if ($Live.Units.ContainsKey("$($pkg.Name)\$($unit.Name)")) { continue }
                # Cargo pre-creates some empty unit folders on every build; they cost nothing
                if (-not (Get-ChildItem $unit.FullName -Recurse -File -Force | Select-Object -First 1)) { continue }
                Remove-Item-Safe $unit.FullName
                $removed++
            }
            if (-not $DryRun -and -not (Get-ChildItem $pkg.FullName -Force)) { Remove-Item -LiteralPath $pkg.FullName -Force }
        }
    }
    # copied outputs (and their .d files) of targets that no longer exist
    $liveStems = @{}
    foreach ($f in $Live.Files.Keys) { if ($f) { $liveStems[[IO.Path]::Combine((Split-Path $f), [IO.Path]::GetFileNameWithoutExtension($f))] = $true } }
    foreach ($dir in @($profileDir, (Join-Path $profileDir 'examples'))) {
        if (-not (Test-Path $dir)) { continue }
        foreach ($f in Get-ChildItem $dir -File -Force) {
            if ($f.Name.StartsWith('.')) { continue }
            $stem = [IO.Path]::Combine($f.DirectoryName, [IO.Path]::GetFileNameWithoutExtension($f.Name))
            if (-not $Live.Files.ContainsKey($f.FullName) -and -not $liveStems.ContainsKey($stem)) { Remove-Item-Safe $f.FullName; $removed++ }
        }
    }
    # incremental caches of units not compiled for a while
    $inc = Join-Path $profileDir 'incremental'
    if (Test-Path $inc) {
        $cutoff = (Get-Date).AddDays(-$StaleDays)
        foreach ($d in Get-ChildItem $inc -Directory) {
            $newest = Get-ChildItem $d.FullName -Recurse -Force -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
            if (-not $newest -or $newest.LastWriteTime -lt $cutoff) { Remove-Item-Safe $d.FullName; $removed++ }
        }
    }
    return $removed
}

$all = @('--workspace', '--all-targets')
try {
    if (Test-Path (Join-Path $target 'debug')) {
        $commands = [System.Collections.Generic.List[string[]]]::new()
        foreach ($sub in 'build', 'clippy', 'check') { $commands.Add([string[]](@($sub) + $all)) }
        $live = Get-LiveSet 'debug' $commands
        $n = Invoke-Sweep 'debug' $live
        "debug: $($live.Units.Count) live units, $n stale items $(if ($DryRun) { 'found' } else { 'deleted' })"
    }
    $release = Join-Path $target 'release'
    if (Test-Path $release) {
        if ($IncludeRelease) {
            $commands = [System.Collections.Generic.List[string[]]]::new()
            $commands.Add([string[]]@('build', '--release', '--workspace'))
            $live = Get-LiveSet 'release' $commands
            $n = Invoke-Sweep 'release' $live
            "release: $($live.Units.Count) live units, $n stale items $(if ($DryRun) { 'found' } else { 'deleted' })"
        } else {
            $newest = Get-ChildItem $release -Recurse -File -Force -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
            if (-not $newest -or $newest.LastWriteTime -lt (Get-Date).AddDays(-$StaleDays)) {
                Remove-Item-Safe $release; "release: unused for $StaleDays days, deleted"
            } else {
                "release: kept (used within $StaleDays days; -IncludeRelease sweeps it)"
            }
        }
    }
    $tmp = Join-Path $target 'tmp'
    if (Test-Path $tmp) {
        Get-ChildItem $tmp -Force | Where-Object { $_.LastWriteTime -lt (Get-Date).AddDays(-1) } | ForEach-Object { Remove-Item-Safe $_.FullName }
    }
} catch {
    "skipped: $($_.Exception.Message); nothing was deleted from the profile that failed"
    exit 2
}

$after = Show-Status 'after:  '
$after[0]
exit (Test-Limits $after[1])
