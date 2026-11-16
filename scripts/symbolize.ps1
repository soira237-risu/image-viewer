<#
.SYNOPSIS
Resolves the frames of a crash.log written by a released build with the release's symbols zip.

.DESCRIPTION
A released image-viewer.exe ships without its .pdb, so the frames of its own code in crash.log
have no names. The report records instead where the executable was loaded and each such frame
as an offset into it ("[image-viewer.exe+0x1a2b3]"), plus the link time stamp and the .pdb's
GUID and age. This script looks those offsets up with Windows' own dbghelp.dll (no debugger to
install) and prints the report with "=> function  file:line" after each resolved frame.

It needs the image-viewer.exe of the same release (the main zip) and image_viewer.pdb (the
-symbols.zip). dbghelp only accepts a .pdb whose GUID matches the executable, and the script
warns when the executable is not the one the report came from.

.EXAMPLE
pwsh -File scripts/symbolize.ps1 -CrashLog "$env:LOCALAPPDATA\image-viewer\crash.log" -Exe C:\rel\image-viewer.exe -Symbols C:\rel\symbols
#>
param(
    [Parameter(Mandatory)] [string]$CrashLog,
    # image-viewer.exe of the release that crashed
    [Parameter(Mandatory)] [string]$Exe,
    # the folder holding image_viewer.pdb (the extracted -symbols.zip); default: the folder of -Exe
    [string]$Symbols
)
$ErrorActionPreference = 'Stop'

$Exe = (Resolve-Path -LiteralPath $Exe).Path
$Symbols = if ($Symbols) { (Resolve-Path -LiteralPath $Symbols).Path } else { Split-Path -Parent $Exe }
$report = Get-Content -LiteralPath $CrashLog

# the report's executable is this one: same link time stamp (PE header, same in file and memory)
$bytes = [IO.File]::ReadAllBytes($Exe)
$pe = [BitConverter]::ToInt32($bytes, 0x3c)
$stamp = '0x{0:x8}' -f [BitConverter]::ToUInt32($bytes, $pe + 8)
$imageLine = $report | Where-Object { $_ -match '^image: ' } | Select-Object -First 1
if ($imageLine -and $imageLine -match 'timestamp (0x[0-9a-f]+)' -and $Matches[1] -ne $stamp) {
    Write-Warning "crash.log is from another build (timestamp $($Matches[1]), $(Split-Path -Leaf $Exe) has $stamp); the names below would be wrong"
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class IvSymbols {
    [DllImport("dbghelp.dll", SetLastError = true)]
    static extern uint SymSetOptions(uint options);
    [DllImport("dbghelp.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool SymInitializeW(IntPtr process, string searchPath, bool invade);
    [DllImport("dbghelp.dll", SetLastError = true)]
    static extern bool SymCleanup(IntPtr process);
    [DllImport("dbghelp.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern ulong SymLoadModuleExW(IntPtr process, IntPtr file, string image, string module,
        ulong baseOfDll, uint dllSize, IntPtr data, uint flags);
    [DllImport("dbghelp.dll", SetLastError = true)]
    static extern bool SymFromAddrW(IntPtr process, ulong address, out ulong displacement, IntPtr symbol);
    [StructLayout(LayoutKind.Sequential)]
    struct LineW64 { public uint SizeOfStruct; public IntPtr Key; public uint LineNumber; public IntPtr FileName; public ulong Address; }
    [DllImport("dbghelp.dll", SetLastError = true)]
    static extern bool SymGetLineFromAddrW64(IntPtr process, ulong address, out uint displacement, ref LineW64 line);

    // any unique non-zero value identifies the session when no process is attached
    static readonly IntPtr Session = new IntPtr(0x1F00D);
    const ulong Base = 0x140000000;

    public static void Load(string symbolPath, string exe) {
        // SYMOPT_UNDNAME | SYMOPT_LOAD_LINES | SYMOPT_FAIL_CRITICAL_ERRORS | SYMOPT_EXACT_SYMBOLS
        // (a .pdb of another build is refused) | SYMOPT_NO_PROMPTS. Not SYMOPT_DEFERRED_LOADS:
        // with no real process behind the session, the deferred load never finds the module.
        SymSetOptions(0x2 | 0x10 | 0x200 | 0x400 | 0x80000);
        if (!SymInitializeW(Session, symbolPath, false))
            throw new Exception("SymInitialize failed: " + Marshal.GetLastWin32Error());
        if (SymLoadModuleExW(Session, IntPtr.Zero, exe, null, Base, 0, IntPtr.Zero, 0) == 0)
            throw new Exception("SymLoadModuleEx failed for " + exe + ": " + Marshal.GetLastWin32Error());
    }

    public static void Unload() { SymCleanup(Session); }

    // The function and line at an offset into the image, or null.
    public static string Resolve(ulong offset) {
        // a frame address is a return address: the call is the instruction before it
        ulong address = Base + (offset > 0 ? offset - 1 : 0);
        const int maxName = 1024, nameAt = 84, size = 88; // SYMBOL_INFOW
        IntPtr info = Marshal.AllocHGlobal(size + 2 * maxName);
        try {
            for (int i = 0; i < size; i += 4) Marshal.WriteInt32(info, i, 0);
            Marshal.WriteInt32(info, 0, size);
            Marshal.WriteInt32(info, 80, maxName);
            ulong displacement;
            if (!SymFromAddrW(Session, address, out displacement, info)) return null;
            int length = Math.Min(Marshal.ReadInt32(info, 76), maxName);
            string name = Marshal.PtrToStringUni(IntPtr.Add(info, nameAt), length);
            LineW64 line = new LineW64();
            line.SizeOfStruct = (uint)Marshal.SizeOf(typeof(LineW64));
            uint lineDisplacement;
            if (SymGetLineFromAddrW64(Session, address, out lineDisplacement, ref line))
                return name + "  " + Marshal.PtrToStringUni(line.FileName) + ":" + line.LineNumber;
            return name;
        } finally {
            Marshal.FreeHGlobal(info);
        }
    }
}
'@

$module = [regex]::Escape((Split-Path -Leaf $Exe))
$frame = [regex]"\[$module\+0x(?<offset>[0-9a-fA-F]+)\]"
$resolved = 0
$frames = 0
[IvSymbols]::Load($Symbols, $Exe)
try {
    foreach ($line in $report) {
        $m = $frame.Match($line)
        if (-not $m.Success) { $line; continue }
        $frames++
        $name = [IvSymbols]::Resolve([Convert]::ToUInt64($m.Groups['offset'].Value, 16))
        if ($name) { $resolved++; "$line  => $name" } else { $line }
    }
} finally {
    [IvSymbols]::Unload()
}
if ($frames -eq 0) {
    Write-Warning "no [$(Split-Path -Leaf $Exe)+0x...] frames in $CrashLog (an older report, or one from another executable)"
} elseif ($resolved -eq 0) {
    $pdb = if ($imageLine -match 'pdb (\S+\.pdb)') { $Matches[1] } else { 'image_viewer.pdb' }
    Write-Warning "none of $frames frames resolved: is the matching $pdb in $Symbols?"
}
