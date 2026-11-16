<#
.SYNOPSIS
Builds the release package: image-viewer-<version>-x86_64-windows.zip with both executables, the
README, CHANGELOG, LICENSE and third-party notices, its SHA-256, and a separate symbols zip.

.DESCRIPTION
The release workflow (.github/workflows/release.yml) runs this on a tag; it also runs locally.
In order, and it stops at the first failure:

1. A LICENSE file must exist in the repository root (the owner chooses the license).
2. The version (Cargo.toml) must match -Tag when given, and CHANGELOG.md must have a section
   for it (it becomes the release notes).
3. The workspace rust-version must be at least every locked dependency's rust-version.
4. A clean release build of both executables in target/package-build, with build paths removed
   from the binaries (--remap-path-prefix for this checkout, the Cargo home and the user
   profile; rustc already embeds only the .pdb's file name). The flags are added with
   --config, so .cargo/config.toml (static C runtime) still applies. The separate target dir
   keeps these differently-flagged units away from target/release. It is deleted when the run
   ends, whether it succeeds, fails at any later step or is interrupted with Ctrl+C, so nothing
   is left to pile up; with -KeepBuild it is kept and marked (.package-kept). While the run is
   going, .package-run in it holds the process id, and scripts/cache.ps1 leaves the folder
   alone; a folder left by a killed run (no live owner, no mark) is deleted by scripts/cache.ps1.
5. Checks of each executable: it imports no C runtime DLL (VCRUNTIME140.dll and the like are not
   part of Windows), contains none of the build machine's paths, and carries the version.
6. scripts/notices.ps1 writes THIRD-PARTY-NOTICES.txt; then the zips and the .sha256.

Output in -OutDir (default target/dist): the zip, <zip>.sha256, the symbols zip (image_viewer.pdb
and iv.pdb, for reading crash backtraces; not for users) and release-notes.md.

.EXAMPLE
pwsh -File scripts/package.ps1 -Jobs 3
pwsh -File scripts/package.ps1 -Tag v0.1.0 -OutDir dist      # as the release workflow runs it
#>
param(
    [string]$OutDir = 'target/dist',
    # the git tag being released (v<version>); checked against Cargo.toml when given
    [string]$Tag,
    [int]$Jobs = 0,
    [switch]$KeepBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$triple = 'x86_64-pc-windows-msvc'

function Fail([string]$Message) {
    if ($env:GITHUB_ACTIONS) { Write-Host "::error::$Message" } else { Write-Host "package: $Message" }
    exit 1
}

# True while the package run that wrote $LockFile (its process id and start time) is running.
# The same check is in scripts/cache.ps1.
function Test-PackageRunLive([string]$LockFile) {
    if (-not (Test-Path -LiteralPath $LockFile)) { return $false }
    try {
        $id, $ticks = (Get-Content -LiteralPath $LockFile -Raw).Trim() -split '\s+'
        $proc = Get-Process -Id ([int]$id) -ErrorAction Stop
    } catch { return $false }
    try { return $proc.StartTime.ToUniversalTime().Ticks -eq [long]$ticks } catch { return $proc.ProcessName -in 'pwsh', 'powershell' }
}

# DLL names in the import (directory 1) and delay-load (directory 13) tables of a PE32+ file
function Get-PeImports([string]$Path) {
    $b = [IO.File]::ReadAllBytes($Path)
    $pe = [BitConverter]::ToInt32($b, 0x3c)
    $count = [BitConverter]::ToUInt16($b, $pe + 6)
    $opt = $pe + 24
    if ([BitConverter]::ToUInt16($b, $opt) -ne 0x20b) { throw "$Path is not a PE32+ image" }
    $table = $opt + [BitConverter]::ToUInt16($b, $pe + 20)
    $sections = foreach ($i in 0..($count - 1)) {
        $s = $table + $i * 40
        [pscustomobject]@{ Va = [BitConverter]::ToUInt32($b, $s + 12); Size = [Math]::Max([BitConverter]::ToUInt32($b, $s + 8), [BitConverter]::ToUInt32($b, $s + 16)); Raw = [BitConverter]::ToUInt32($b, $s + 20) }
    }
    $offset = { param($rva) $s = $sections | Where-Object { $rva -ge $_.Va -and $rva -lt $_.Va + $_.Size } | Select-Object -First 1; if (-not $s) { throw "RVA $rva outside sections" }; $s.Raw + ($rva - $s.Va) }
    $names = @()
    foreach ($d in @(@(1, 20, 12), @(13, 32, 4))) {
        $rva = [BitConverter]::ToUInt32($b, $opt + 112 + $d[0] * 8)
        if ($rva -eq 0) { continue }
        $at = & $offset $rva
        while ([BitConverter]::ToUInt64($b, $at) -ne 0 -or [BitConverter]::ToUInt64($b, $at + 8) -ne 0) {
            $n = & $offset ([BitConverter]::ToUInt32($b, $at + $d[2]))
            $end = [Array]::IndexOf($b, [byte]0, [int]$n)
            $names += [Text.Encoding]::ASCII.GetString($b, $n, $end - $n)
            $at += $d[1]
        }
    }
    return $names
}

$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
$nasm = Join-Path $env:LOCALAPPDATA 'bin\NASM'
if ((Test-Path $nasm) -and -not ($env:PATH -split ';' -contains $nasm)) { $env:PATH = "$nasm;$env:PATH" }

# 1. the project's own license
$licenses = @(Get-ChildItem $root -File | Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING)([-._].*)?$' })
if (-not $licenses) {
    Fail ('No LICENSE file in the repository root. image-viewer has no license yet, and a release ' +
        'without one gives nobody the right to use or redistribute it. The owner must choose a ' +
        'license and add it as LICENSE (or LICENSE-MIT, LICENSE-APACHE, ...), and set `license` ' +
        'under [workspace.package] in Cargo.toml; then tag the release again.')
}

# 2. version, tag, changelog
Push-Location $root
try {
    $meta = & $cargo metadata --format-version 1 --locked --no-deps | ConvertFrom-Json -Depth 64
    if ($LASTEXITCODE -ne 0) { Fail "cargo metadata failed (exit $LASTEXITCODE)" }
    $full = & $cargo metadata --format-version 1 --locked | ConvertFrom-Json -Depth 64
    if ($LASTEXITCODE -ne 0) { Fail "cargo metadata failed (exit $LASTEXITCODE)" }
} finally { Pop-Location }
$viewer = $meta.packages | Where-Object { $_.name -eq 'iv-win' }
$version = $viewer.version
if ($Tag -and $Tag -ne "v$version") { Fail "tag $Tag does not match the version in Cargo.toml ($version); the tag must be v$version" }
if (-not $viewer.license -and -not $viewer.license_file) {
    Write-Host "package: warning: Cargo.toml declares no license (set license under [workspace.package] and license.workspace = true in each crate)"
}
$changelog = Get-Content (Join-Path $root 'CHANGELOG.md') -Encoding utf8
$start = ($changelog | Select-String -Pattern "^## \[$([regex]::Escape($version))\]" | Select-Object -First 1).LineNumber
if (-not $start) { Fail "CHANGELOG.md has no '## [$version]' section" }
$notes = @($changelog | Select-Object -Skip $start | ForEach-Object -Begin { $on = $true } -Process {
        if ($_ -match '^## \[') { $on = $false }
        if ($on) { $_ }
    })

# 3. rust-version is the real floor
$declared = [version]($viewer.rust_version -replace '^(\d+\.\d+)$', '$1.0')
$ids = @{}
foreach ($n in $full.resolve.nodes) { $ids[$n.id] = $true }
$needed = $full.packages | Where-Object { $ids.ContainsKey($_.id) -and $_.rust_version } |
    Sort-Object { [version]($_.rust_version -replace '^(\d+\.\d+)$', '$1.0') } | Select-Object -Last 1
if ([version]($needed.rust_version -replace '^(\d+\.\d+)$', '$1.0') -gt $declared) {
    Fail "rust-version in Cargo.toml is $($viewer.rust_version), but $($needed.name) $($needed.version) needs $($needed.rust_version)"
}

# 4. clean release build without build paths
if ($env:RUSTFLAGS -or $env:CARGO_ENCODED_RUSTFLAGS) {
    Fail 'RUSTFLAGS is set: it replaces the flags of .cargo/config.toml (static C runtime); unset it'
}
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$cargoHome = [IO.Path]::GetFullPath($cargoHome)
# rustc applies the last matching prefix: general first, specific last
$remaps = [ordered]@{}
$remaps[[IO.Path]::GetFullPath($env:USERPROFILE).TrimEnd('\')] = '/home'
$remaps[$cargoHome.TrimEnd('\')] = '/cargo'
$remaps[$root.TrimEnd('\')] = '/image-viewer'
$flags = @($remaps.GetEnumerator() | ForEach-Object { "'--remap-path-prefix=$($_.Key)=$($_.Value)'" })
$buildDir = Join-Path $root 'target\package-build'
# The run marks the build dir as its own (process id and start time) so that scripts/cache.ps1
# leaves it alone, and the finally block below deletes it however the run ends (a failed check,
# a compile error, Ctrl+C), or with -KeepBuild marks it kept. Only a killed run leaves the tree
# behind unmarked, and scripts/cache.ps1 deletes that.
$lockFile = Join-Path $buildDir '.package-run'
$keptFile = Join-Path $buildDir '.package-kept'
if (Test-PackageRunLive $lockFile) { Fail "another package run is using $buildDir" }
New-Item -ItemType Directory -Force $buildDir | Out-Null
if (Test-Path -LiteralPath $keptFile) { Remove-Item -LiteralPath $keptFile -Force }
[IO.File]::WriteAllText($lockFile, "$PID $((Get-Process -Id $PID).StartTime.ToUniversalTime().Ticks)")
$stage = $null
$symStage = $null
try {
    $cargoArgs = @('build', '--release', '--locked', '--workspace', '--bins', '--target-dir', $buildDir,
        '--config', "target.$triple.rustflags=[$($flags -join ', ')]")
    if ($Jobs -gt 0) { $cargoArgs += @('-j', "$Jobs") }
    Push-Location $root
    try {
        & $cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { Fail "release build failed (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
    $bin = Join-Path $buildDir 'release'
    $exes = @('image-viewer.exe', 'iv.exe') | ForEach-Object { Join-Path $bin $_ }

    # 5. checks
    $latin1 = [Text.Encoding]::GetEncoding(28591)
    $leaks = @($remaps.Keys) | ForEach-Object { $_; $_.Replace('\', '/') } | Select-Object -Unique
    foreach ($exe in $exes) {
        if (-not (Test-Path $exe)) { Fail "$exe was not built" }
        $name = Split-Path $exe -Leaf
        $imports = Get-PeImports $exe
        if (-not ($imports | Where-Object { $_ -ieq 'KERNEL32.dll' })) { Fail "$name`: import table not read ($($imports -join ', '))" }
        $crt = @($imports | Where-Object { $_ -match '^(vcruntime|msvcp|msvcr|ucrtbase|api-ms-win-crt-|concrt)' })
        if ($crt) { Fail "$name imports C runtime DLLs that Windows does not ship: $($crt -join ', ') (is the static CRT of .cargo/config.toml in effect?)" }
        $text = $latin1.GetString([IO.File]::ReadAllBytes($exe))
        foreach ($leak in $leaks) {
            $at = $text.IndexOf($leak, [StringComparison]::OrdinalIgnoreCase)
            if ($at -ge 0) {
                $context = ($text.Substring([Math]::Max(0, $at - 40), [Math]::Min(160, $text.Length - [Math]::Max(0, $at - 40))) -replace '[^\x20-\x7e]', '.')
                Fail "$name contains the build path $leak`: ...$context..."
            }
        }
        "package: $name $([Math]::Round((Get-Item $exe).Length / 1MB, 2)) MB, imports $($imports.Count) DLLs ($($imports -join ', ')), no build paths"
    }
    $fileVersion = (Get-Item $exes[0]).VersionInfo.ProductVersion
    if ($fileVersion -ne $version) { Fail "image-viewer.exe has product version '$fileVersion', expected $version" }
    $cliVersion = (& $exes[1] --version) -join ' '
    if ($cliVersion -notmatch [regex]::Escape($version)) { Fail "iv --version printed '$cliVersion', expected $version" }

    # 6. notices and zips
    $out = [IO.Path]::GetFullPath($OutDir, $root)
    $name = "image-viewer-$version-x86_64-windows"
    $stage = Join-Path $out $name
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $stage | Out-Null
    & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'notices.ps1') -Out (Join-Path $stage 'THIRD-PARTY-NOTICES.txt')
    if ($LASTEXITCODE -ne 0) { Fail 'third-party notices could not be written (see above)' }
    Copy-Item $exes $stage
    Copy-Item (Join-Path $root 'README.md'), (Join-Path $root 'CHANGELOG.md') $stage
    Copy-Item $licenses.FullName $stage

    $zip = Join-Path $out "$name.zip"
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::CreateFromDirectory($stage, $zip, [IO.Compression.CompressionLevel]::Optimal, $true)
    $hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$zip.sha256", "$hash  $name.zip`n")

    $symbols = Join-Path $out "$name-symbols.zip"
    $symStage = Join-Path $out "$name-symbols"
    if (Test-Path $symbols) { Remove-Item -Force $symbols }
    if (Test-Path $symStage) { Remove-Item -Recurse -Force $symStage }
    New-Item -ItemType Directory -Force $symStage | Out-Null
    foreach ($pdb in 'image_viewer.pdb', 'iv.pdb') {
        $p = Join-Path $bin $pdb
        if (-not (Test-Path $p)) { Fail "$pdb was not produced" }
        Copy-Item $p $symStage
    }
    [IO.Compression.ZipFile]::CreateFromDirectory($symStage, $symbols, [IO.Compression.CompressionLevel]::Optimal, $false)
    Remove-Item -Recurse -Force $stage, $symStage

    $notesFile = Join-Path $out 'release-notes.md'
    [IO.File]::WriteAllText($notesFile, (($notes -join "`n").Trim() + "`n`nSHA-256 ($name.zip): ``$hash```n"), [Text.UTF8Encoding]::new($false))
} finally {
    # the staging folders are intermediate; a failure between creating and zipping them leaves them
    foreach ($dir in @($stage, $symStage)) {
        if ($dir -and (Test-Path -LiteralPath $dir)) { Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction Continue }
    }
    try {
        if ($KeepBuild) {
            [IO.File]::WriteAllText($keptFile, '')
            Remove-Item -LiteralPath $lockFile -Force
            Write-Host "package: kept $buildDir (-KeepBuild)"
        } else {
            Remove-Item -LiteralPath $buildDir -Recurse -Force
        }
    } catch {
        Write-Host "package: warning: could not clean up $buildDir ($($_.Exception.Message)); scripts/cache.ps1 deletes it later"
    }
}
"package: $zip ($([Math]::Round((Get-Item $zip).Length / 1MB, 2)) MB), sha256 $hash"
"package: $symbols ($([Math]::Round((Get-Item $symbols).Length / 1MB, 2)) MB), $notesFile"
