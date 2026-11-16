<#
.SYNOPSIS
Writes THIRD-PARTY-NOTICES.txt: the license of every package built into the release executables,
with the license texts, read from Cargo.lock and the local Cargo registry. No extra tools.

.DESCRIPTION
The packages are those `cargo metadata` resolves for x86_64-pc-windows-msvc, reached from the two
executables' crates (iv-win, iv-cli) through normal dependencies only: build scripts, proc macros
and dev-dependencies run on the build machine and are not part of the executables. Our own
workspace crates are left out.

For each package the texts are the license files in its registry folder (LICENSE*, COPYING*,
NOTICE*, PATENTS*, COPYRIGHT*, UNLICENSE*), also in a `vendor` or `third_party` subfolder for
vendored C code (libwebp-sys), plus the file named by `license-file`. A package that ships none
gets the standard text of a license it offers (MIT/BSD with its authors as the copyright holders,
or a text from the Rust toolchain's share/doc/rust/licenses). Identical texts are printed once,
with every package that uses them.

The Rust standard library is linked in too: its notices (COPYRIGHT-library.html from the toolchain)
are copied next to the output as RUST-STD-COPYRIGHT.html.

Fails (exit 1) when a package declares a license outside the allow-list below or has no text at
all, so a new dependency with a new license is looked at before it ships.

.EXAMPLE
pwsh -File scripts/notices.ps1 -Out target/dist/THIRD-PARTY-NOTICES.txt
#>
param(
    [Parameter(Mandatory)][string]$Out,
    [string]$Target = 'x86_64-pc-windows-msvc'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

# Licenses accepted for code inside the executables. MPL-2.0 is file-level copyleft: unmodified
# use is fine, recipients must be told where the source is (the "Source" line of each package).
$allowed = @(
    'MIT', 'MIT-0', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause', '0BSD', 'Zlib', 'ISC',
    'Unicode-3.0', 'Unicode-DFS-2016', 'Unlicense', 'CC0-1.0', 'BSL-1.0', 'MPL-2.0', 'LLVM-exception'
)
$executableCrates = @('iv-win', 'iv-cli')
$licenseFile = '^(LICEN[CS]E|COPYING|NOTICE|PATENTS|COPYRIGHT|UNLICENSE)([-._].*)?$'

$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
$rustc = (Get-Command rustc -ErrorAction SilentlyContinue).Source
if (-not $rustc) { $rustc = Join-Path $env:USERPROFILE '.cargo\bin\rustc.exe' }

Push-Location $root
try {
    $json = & $cargo metadata --format-version 1 --locked --filter-platform $Target
    if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed (exit $LASTEXITCODE)" }
} finally { Pop-Location }
$meta = $json | ConvertFrom-Json -Depth 64
$sysroot = (& $rustc --print sysroot).Trim()
$rustDoc = Join-Path $sysroot 'share\doc\rust'
$rustVersion = (& $rustc --version).Trim()

$packages = @{}
foreach ($p in $meta.packages) { $packages[$p.id] = $p }
$nodes = @{}
foreach ($n in $meta.resolve.nodes) { $nodes[$n.id] = $n }

function Test-ProcMacro($p) { return [bool]($p.targets | Where-Object { $_.kind -contains 'proc-macro' }) }

# walk normal dependency edges from the executables' crates
$shipped = @{}
$queue = [System.Collections.Generic.Queue[string]]::new()
foreach ($p in $meta.packages | Where-Object { $_.name -in $executableCrates -and $null -eq $_.source }) { $queue.Enqueue($p.id) }
if ($queue.Count -ne $executableCrates.Count) { throw "workspace crates $($executableCrates -join ', ') not found" }
while ($queue.Count) {
    $id = $queue.Dequeue()
    if ($shipped.ContainsKey($id)) { continue }
    $shipped[$id] = $true
    foreach ($dep in $nodes[$id].deps) {
        if (-not ($dep.dep_kinds | Where-Object { $null -eq $_.kind })) { continue }
        if (Test-ProcMacro $packages[$dep.pkg]) { continue }
        $queue.Enqueue($dep.pkg)
    }
}

# the license identifiers of an SPDX expression ("MIT/Apache-2.0" is the old spelling of OR)
function Get-LicenseIds([string]$expr) {
    return @(($expr -replace '[()]', ' ' -replace '/', ' OR ') -split '\s+' |
        Where-Object { $_ -and $_ -notin 'AND', 'OR', 'WITH' } | Select-Object -Unique)
}

# standard texts for a package that ships no license file
function Get-StandardText([string]$id, $p) {
    $holders = if ($p.authors) { ($p.authors | ForEach-Object { $_ -replace '\s*<[^>]*>', '' }) -join ', ' } else { "the $($p.name) authors" }
    switch ($id) {
        'MIT' {
            $t = Get-Content -Raw (Join-Path $rustDoc 'licenses\MIT.txt')
            return $t.Replace('<year> <copyright holders>', $holders)
        }
        'BSD-3-Clause' {
            return @"
Copyright (c) $holders

Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote products derived from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
"@
        }
        default {
            $file = Join-Path $rustDoc "licenses\$id.txt"
            if (Test-Path $file) {
                $t = Get-Content -Raw $file
                return $t.Replace('<year> <owner>', $holders).Replace('<copyright notice>', "Copyright (c) $holders")
            }
            return $null
        }
    }
}

function Get-Normalized([string]$text) {
    return (($text -replace "`r`n", "`n" -replace "`r", "`n").TrimStart([char]0xFEFF).Trim()) + "`n"
}

$problems = @()
$rows = @()
# normalized text -> list of "package version (file)"
$texts = [ordered]@{}
# in name order, so the same Cargo.lock always gives the same file
foreach ($id in ($shipped.Keys | Sort-Object { $packages[$_].name }, { $packages[$_].version })) {
    $p = $packages[$id]
    if ($null -eq $p.source) { continue } # our own crates
    $label = "$($p.name) $($p.version)"
    $expr = $p.license
    if (-not $expr -and -not $p.license_file) { $problems += "$label declares no license"; continue }
    if ($expr) {
        $bad = @(Get-LicenseIds $expr | Where-Object { $_ -notin $allowed })
        if ($bad) { $problems += "$label is licensed $expr ($($bad -join ', ') not in the allow-list of scripts/notices.ps1)"; continue }
    }
    $dir = Split-Path $p.manifest_path
    # the package's own license: its files, or the file `license-file` names
    $own = @(Get-ChildItem $dir -File | Where-Object { $_.Name -match $licenseFile })
    if ($p.license_file) {
        $named = Join-Path $dir $p.license_file
        if (Test-Path $named) { $own += Get-Item $named } else { $problems += "$label names license-file $($p.license_file), which is not in the package" }
    }
    # licenses of C code vendored inside it
    $vendored = @()
    foreach ($sub in 'vendor', 'third_party') {
        $folder = Join-Path $dir $sub
        if (Test-Path $folder) { $vendored += @(Get-ChildItem $folder -File | Where-Object { $_.Name -match $licenseFile }) }
    }
    $entries = @()
    foreach ($f in @(@($own) + @($vendored) | Sort-Object FullName -Unique)) {
        $entries += @{ Name = $f.FullName.Substring($dir.Length + 1).Replace('\', '/'); Text = Get-Content -Raw -Encoding utf8 $f.FullName }
    }
    if (-not $own) {
        # no file of its own: the standard text of the first license it offers that we have
        $standard = $null
        foreach ($lid in Get-LicenseIds $expr) {
            $t = Get-StandardText $lid $p
            if ($t) { $standard = @{ Name = "$lid (standard text; the package ships no license file)"; Text = $t }; break }
        }
        if (-not $standard) { $problems += "$label ($expr) ships no license file and no standard text is known"; continue }
        $entries += $standard
    }
    foreach ($e in $entries) {
        if (-not $e.Text -or -not $e.Text.Trim()) { continue }
        $key = Get-Normalized $e.Text
        if (-not $texts.Contains($key)) { $texts[$key] = [System.Collections.Generic.List[string]]::new() }
        $texts[$key].Add("$label ($($e.Name))")
    }
    $source = if ($p.repository) { $p.repository } else { "https://crates.io/crates/$($p.name)" }
    $rows += [pscustomobject]@{ Name = $p.name; Version = $p.version; License = $(if ($expr) { $expr } else { "see $($p.license_file)" }); Source = $source }
}

if ($problems) {
    $problems | Sort-Object | ForEach-Object { Write-Host "notices: $_" }
    Write-Host "notices: $($problems.Count) package(s) need attention; nothing written"
    exit 1
}

$rows = $rows | Sort-Object Name, { [string]$_.Version }
$version = ($meta.packages | Where-Object { $_.name -eq 'iv-win' -and $null -eq $_.source }).version
$nl = "`n"
$sb = [System.Text.StringBuilder]::new()
$rule = '=' * 78
[void]$sb.Append("image-viewer $version - third-party notices$nl$nl")
[void]$sb.Append("image-viewer.exe and iv.exe contain the open-source packages listed below (Rust crates from$nl")
[void]$sb.Append("crates.io, built from the versions in Cargo.lock), and the Rust standard library, whose notices$nl")
[void]$sb.Append("are in RUST-STD-COPYRIGHT.html. Each package keeps its own license; the license texts follow$nl")
[void]$sb.Append("the list. Source code of every package is available at the address given for it and at$nl")
[void]$sb.Append("https://crates.io/crates/<name>/<version>.$nl$nl")
[void]$sb.Append("Compiler: $rustVersion$nl$nl")
[void]$sb.Append("이 프로그램(image-viewer.exe, iv.exe)에는 아래의 오픈 소스 패키지와 Rust 표준 라이브러리가$nl")
[void]$sb.Append("들어 있습니다. 각 패키지의 라이선스 전문은 목록 뒤에 있습니다.$nl$nl")
[void]$sb.Append("$rule$nl" + "Packages ($($rows.Count))$nl$rule$nl$nl")
$w = ($rows | ForEach-Object { "$($_.Name) $($_.Version)".Length } | Measure-Object -Maximum).Maximum
foreach ($r in $rows) {
    [void]$sb.Append(("{0,-$w}  {1}$nl{2,-$w}  Source: {3}$nl" -f "$($r.Name) $($r.Version)", $r.License, '', $r.Source))
}
[void]$sb.Append("$nl$rule$nl" + "License texts ($($texts.Count) distinct)$nl$rule$nl")
foreach ($key in $texts.Keys) {
    [void]$sb.Append("$nl$('-' * 78)$nl")
    foreach ($user in ($texts[$key] | Sort-Object)) { [void]$sb.Append("Used by: $user$nl") }
    [void]$sb.Append("$('-' * 78)$nl$nl")
    [void]$sb.Append($key)
}

$outFull = [IO.Path]::GetFullPath($Out, (Get-Location).Path)
New-Item -ItemType Directory -Force (Split-Path $outFull) | Out-Null
[IO.File]::WriteAllText($outFull, $sb.ToString().Replace("`n", "`r`n"), [Text.UTF8Encoding]::new($false))
$std = Join-Path $rustDoc 'COPYRIGHT-library.html'
if (-not (Test-Path $std)) { Write-Host "notices: $std not found (the rustc component installs it)"; exit 1 }
Copy-Item $std (Join-Path (Split-Path $outFull) 'RUST-STD-COPYRIGHT.html') -Force
"notices: $($rows.Count) packages, $($texts.Count) distinct license texts -> $outFull"
