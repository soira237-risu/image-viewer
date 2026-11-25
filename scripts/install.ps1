<#
.SYNOPSIS
Installs, upgrades or removes image-viewer for the current user, from a release zip.

.DESCRIPTION
No administrator rights: everything goes under the current user.

Installing (or upgrading over an earlier install):
1. The files: those next to this script when it is run from an unzipped release (the zip
   carries it); otherwise the release zip: -Zip, or the latest GitHub release (-Version for
   another tag), downloaded with its .sha256 and checked against it. A local zip is checked too
   when its .sha256 is next to it. Run from the install folder itself, it upgrades to the
   latest release.
2. Its files are copied into -Dir (default %LOCALAPPDATA%\Programs\image-viewer). A viewer still
   running from there stops the install. The thumbnail DLL may be held by Explorer's thumbnail
   process (dllhost.exe): the old file is then renamed aside and replaced, and removed by the
   next install or the uninstall.
3. The viewer registers itself: "Open with" and Default apps (--register) and Explorer
   thumbnails (--register-thumbnails, unless -NoThumbnails).
4. A Start menu shortcut, and an entry in Settings > Apps > Installed apps whose Uninstall runs
   this script (copied into -Dir) with -Uninstall.
5. Settings > Default apps opens on the viewer's page (unless -NoDefaultApps): Windows lets only
   the user pick the default program, with its "Set default" button.

-Uninstall undoes all of that; settings and logs stay unless -RemoveSettings.

Runs on Windows PowerShell 5.1 and PowerShell 7.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File install.ps1                 # in an unzipped release: that one; elsewhere: the latest
powershell -ExecutionPolicy Bypass -File install.ps1 -Version v0.2.0
powershell -ExecutionPolicy Bypass -File install.ps1 -Zip image-viewer-0.2.0-x86_64-windows.zip
powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [string]$Zip,
    [string]$Version,
    [string]$Dir = (Join-Path $env:LOCALAPPDATA 'Programs\image-viewer'),
    [switch]$NoThumbnails,
    [switch]$NoDefaultApps,
    [switch]$Uninstall,
    [switch]$RemoveSettings,
    # wait for Enter before closing (Installed apps starts the uninstall in a window of its own)
    [switch]$Wait,
    [string]$Repo = 'soira237-risu/image-viewer',
    # for tests: where the shortcut and the Installed apps entry go, and whether the viewer
    # registers itself
    [string]$StartMenu = (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    [string]$AppsKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\image-viewer',
    [switch]$NoRegister
)

$ErrorActionPreference = 'Stop'
$exeName = 'image-viewer.exe'
$dllName = 'image_viewer_thumbs.dll'
$shortcut = Join-Path $StartMenu 'image-viewer.lnk'

function Say([string]$Text) { Write-Host "image-viewer: $Text" }
function Done([int]$Code) {
    if ($Wait) { [void](Read-Host 'Press Enter to close') }
    exit $Code
}
function Fail([string]$Text) { Write-Host "image-viewer: $Text" -ForegroundColor Red; Done 1 }

function Get-FullPath([string]$Path) {
    $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path).TrimEnd('\')
}

# viewers running from the install folder (their files cannot be replaced or deleted)
function Get-RunningViewers([string]$Folder) {
    $prefix = (Get-FullPath $Folder) + '\'
    @(Get-Process -Name 'image-viewer', 'iv' -ErrorAction SilentlyContinue | Where-Object {
            try { $_.Path -and $_.Path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) } catch { $false }
        })
}

# runs the installed viewer with one flag and waits; it is a windowed program, so its exit code
# comes through Start-Process
function Invoke-Viewer([string]$Flag) {
    $exe = Join-Path $Dir $exeName
    $p = Start-Process -FilePath $exe -ArgumentList $Flag -Wait -PassThru -WindowStyle Hidden
    if ($p.ExitCode -ne 0) { throw "$exeName $Flag failed (exit $($p.ExitCode))" }
}

# files renamed aside by an earlier run because Explorer held them
function Remove-Leftovers([string]$Folder) {
    Get-ChildItem -LiteralPath $Folder -File -Filter '*.old-*' -ErrorAction SilentlyContinue | ForEach-Object {
        try { Remove-Item -LiteralPath $_.FullName -Force } catch { }
    }
}

# copies $Source over $Target; a target held open by another process (the thumbnail DLL in
# dllhost.exe) is renamed aside first, which Windows allows for a loaded DLL
function Copy-Replacing([string]$Source, [string]$Target) {
    try {
        Copy-Item -LiteralPath $Source -Destination $Target -Force
    } catch {
        if (-not (Test-Path -LiteralPath $Target)) { throw }
        $aside = "$Target.old-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
        Move-Item -LiteralPath $Target -Destination $aside
        Copy-Item -LiteralPath $Source -Destination $Target -Force
        Say "$(Split-Path $Target -Leaf) was in use; the old copy is removed by the next install or uninstall"
    }
}

function Get-ReleaseZip([string]$Work) {
    if ($Zip) {
        $path = Get-FullPath $Zip
        if (-not (Test-Path -LiteralPath $path)) { Fail "$path not found" }
        $sum = "$path.sha256"
        if (Test-Path -LiteralPath $sum) { Test-Sha256 $path $sum } else { Say "no $(Split-Path $sum -Leaf) next to the zip: not checked" }
        return $path
    }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $api = if ($Version) { "https://api.github.com/repos/$Repo/releases/tags/$Version" } else { "https://api.github.com/repos/$Repo/releases/latest" }
    $headers = @{ 'User-Agent' = 'image-viewer-install'; 'Accept' = 'application/vnd.github+json' }
    $release = Invoke-RestMethod -Uri $api -Headers $headers -UseBasicParsing
    $asset = @($release.assets | Where-Object { $_.name -match '^image-viewer-.*-x86_64-windows\.zip$' }) | Select-Object -First 1
    $sumAsset = @($release.assets | Where-Object { $_.name -eq "$($asset.name).sha256" }) | Select-Object -First 1
    if (-not $asset -or -not $sumAsset) { Fail "release $($release.tag_name) has no zip with a .sha256" }
    $path = Join-Path $Work $asset.name
    $sum = "$path.sha256"
    Say "downloading $($asset.name) ($([Math]::Round($asset.size / 1MB, 1)) MB) from release $($release.tag_name)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $path -Headers $headers -UseBasicParsing
    Invoke-WebRequest -Uri $sumAsset.browser_download_url -OutFile $sum -Headers $headers -UseBasicParsing
    Test-Sha256 $path $sum
    return $path
}

function Test-Sha256([string]$Path, [string]$SumFile) {
    $expected = ((Get-Content -LiteralPath $SumFile -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) { Fail "SHA-256 of $(Split-Path $Path -Leaf) is $actual, the release says $expected" }
    Say "SHA-256 checked: $actual"
}

function Install-Viewer {
    $running = Get-RunningViewers $Dir
    if ($running) { Fail "close image-viewer first (running from $Dir, process $($running.Id -join ', '))" }
    $work = Join-Path ([IO.Path]::GetTempPath()) "image-viewer-install-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
    New-Item -ItemType Directory -Force $work | Out-Null
    try {
        # run from an unzipped release (the zip carries this script): install that folder
        $here = if ($PSScriptRoot) { Get-FullPath $PSScriptRoot } else { '' }
        if (-not $Zip -and -not $Version -and $here -and -not ($here -ieq $Dir) -and (Test-Path -LiteralPath (Join-Path $here $exeName))) {
            $from = $here
            Say "installing the files next to this script ($from)"
        } else {
            $zipPath = Get-ReleaseZip $work
            $unpacked = Join-Path $work 'unpacked'
            Expand-Archive -LiteralPath $zipPath -DestinationPath $unpacked
            # the zip holds one folder, image-viewer-<version>-x86_64-windows
            $exe = @(Get-ChildItem -LiteralPath $unpacked -Recurse -File -Filter $exeName) | Select-Object -First 1
            if (-not $exe) { Fail "$exeName is not in $(Split-Path $zipPath -Leaf)" }
            $from = $exe.DirectoryName
        }
        New-Item -ItemType Directory -Force $Dir | Out-Null
        Remove-Leftovers $Dir
        foreach ($f in Get-ChildItem -LiteralPath $from -File) {
            Copy-Replacing $f.FullName (Join-Path $Dir $f.Name)
        }
        # this script stays with the program: Installed apps runs it to uninstall
        if ($PSCommandPath -and -not ((Get-FullPath $PSCommandPath) -ieq (Get-FullPath (Join-Path $Dir 'install.ps1')))) {
            Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $Dir 'install.ps1') -Force
        }
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
    $exePath = Join-Path $Dir $exeName
    $version = (Get-Item -LiteralPath $exePath).VersionInfo.ProductVersion
    Say "installed $version in $Dir"

    if (-not $NoRegister) {
        Invoke-Viewer '--register'
        Say 'listed in "Open with" and Settings > Default apps'
        if ($NoThumbnails) {
            if (Test-Path (Join-Path $Dir $dllName)) { Invoke-Viewer '--unregister-thumbnails' }
        } elseif (Test-Path (Join-Path $Dir $dllName)) {
            Invoke-Viewer '--register-thumbnails'
            Say 'Explorer thumbnails registered for the formats Windows cannot draw'
        }
    }

    New-Item -ItemType Directory -Force $StartMenu | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($shortcut)
    $link.TargetPath = $exePath
    $link.WorkingDirectory = $Dir
    $link.IconLocation = "$exePath,0"
    $link.Description = 'image-viewer'
    $link.Save()

    $ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $sizeKb = [int]((Get-ChildItem -LiteralPath $Dir -File | Measure-Object -Property Length -Sum).Sum / 1KB)
    New-Item -Path $AppsKey -Force | Out-Null
    $values = [ordered]@{
        DisplayName     = 'image-viewer'
        DisplayVersion  = $version
        Publisher       = ($Repo -split '/')[0]
        DisplayIcon     = "$exePath,0"
        InstallLocation = $Dir
        URLInfoAbout    = "https://github.com/$Repo"
        UninstallString = "`"$ps`" -NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $Dir 'install.ps1')`" -Uninstall -Wait"
    }
    foreach ($k in $values.Keys) { New-ItemProperty -Path $AppsKey -Name $k -Value $values[$k] -PropertyType String -Force | Out-Null }
    foreach ($k in 'NoModify', 'NoRepair') { New-ItemProperty -Path $AppsKey -Name $k -Value 1 -PropertyType DWord -Force | Out-Null }
    New-ItemProperty -Path $AppsKey -Name 'EstimatedSize' -Value $sizeKb -PropertyType DWord -Force | Out-Null
    Say 'Start menu shortcut and Installed apps entry added'

    if (-not $NoDefaultApps -and -not $NoRegister) {
        Start-Process 'ms-settings:defaultapps?registeredAppUser=image-viewer'
        Say 'to make it the default image program, press "Set default" on the Default apps page that opened'
    }
}

function Uninstall-Viewer {
    $running = Get-RunningViewers $Dir
    if ($running) { Fail "close image-viewer first (running from $Dir, process $($running.Id -join ', '))" }
    $exePath = Join-Path $Dir $exeName
    if (-not $NoRegister -and (Test-Path -LiteralPath $exePath)) {
        Invoke-Viewer '--unregister-thumbnails'
        Invoke-Viewer '--unregister'
        Say 'removed from "Open with", Default apps and Explorer thumbnails'
    }
    if (Test-Path -LiteralPath $shortcut) { Remove-Item -LiteralPath $shortcut -Force }
    if (Test-Path $AppsKey) { Remove-Item -Path $AppsKey -Recurse -Force }
    if (Test-Path -LiteralPath $Dir) {
        $left = @()
        foreach ($f in Get-ChildItem -LiteralPath $Dir -File) {
            try {
                Remove-Item -LiteralPath $f.FullName -Force
            } catch {
                # held by Explorer's thumbnail process: moved out of the folder into the temp folder
                try {
                    Move-Item -LiteralPath $f.FullName -Destination (Join-Path ([IO.Path]::GetTempPath()) "$($f.Name).old-$([Guid]::NewGuid().ToString('N').Substring(0, 8))")
                } catch { $left += $f.Name }
            }
        }
        try { Remove-Item -LiteralPath $Dir -Recurse -Force } catch { $left += $Dir }
        if ($left) { Say "could not remove: $($left -join ', ') (in use; delete it after signing out)" }
    }
    if ($RemoveSettings) {
        foreach ($d in (Join-Path $env:APPDATA 'image-viewer'), (Join-Path $env:LOCALAPPDATA 'image-viewer')) {
            if (Test-Path -LiteralPath $d) { Remove-Item -LiteralPath $d -Recurse -Force }
        }
        Say 'settings and logs removed'
    }
    Say "uninstalled from $Dir"
}

$Dir = Get-FullPath $Dir
try {
    if ($Uninstall) { Uninstall-Viewer } else { Install-Viewer }
} catch {
    Fail $_.Exception.Message
}
Done 0
