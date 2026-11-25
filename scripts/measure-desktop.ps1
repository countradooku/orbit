<#
.SYNOPSIS
  Measures a desktop app the way a user experiences it: time until its window
  is up, and the memory and idle CPU of its whole process tree.

.DESCRIPTION
  Electron apps (T3 Code) run a main process plus renderer, GPU and utility
  processes, so every number here sums the full tree, found through parent
  process ids. Idle CPU is measured after the app settles, as a percentage of
  one core.

  Orbit's background host (orbitd) is started by the app and is in its tree.
  A host already running for the same data folder (ORBIT_DATA, matched in its
  command line) is counted too, as a companion, with its descendants; the
  host's own share is reported as HostWorkingMB / HostPrivateMB. Companions
  are matched only when ORBIT_DATA is set, so a measurement never touches the
  host of the user's own data.

.EXAMPLE
  ./scripts/measure-desktop.ps1 -Label "Orbit (GPUI Fast)" -Exe target/release/orbit-desktop.exe
#>
param(
    [Parameter(Mandatory)] [string] $Label,
    [Parameter(Mandatory)] [string] $Exe,
    [string[]] $Arguments = @(),
    [int] $SettleSeconds = 8,
    [int] $IdleSeconds = 10,
    [int] $Runs = 3,
    # Processes counted with the app when they serve the same data folder.
    [string[]] $Companions = @('orbitd')
)

$ErrorActionPreference = 'Stop'

function Get-Tree([int] $RootId) {
    $all = Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId, Name, CommandLine
    $ids = [System.Collections.Generic.HashSet[int]]::new()
    [void]$ids.Add($RootId)
    if ($env:ORBIT_DATA) {
        foreach ($p in $all) {
            $name = $p.Name -replace '\.exe$', ''
            if ($Companions -contains $name -and $p.CommandLine -and $p.CommandLine.Contains($env:ORBIT_DATA)) {
                [void]$ids.Add([int]$p.ProcessId)
            }
        }
    }
    do {
        $added = $false
        foreach ($p in $all) {
            if ($ids.Contains([int]$p.ParentProcessId) -and -not $ids.Contains([int]$p.ProcessId)) {
                [void]$ids.Add([int]$p.ProcessId); $added = $true
            }
        }
    } while ($added)
    foreach ($id in $ids) { Get-Process -Id $id -ErrorAction SilentlyContinue }
}

function Measure-Once {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $Exe -ArgumentList $Arguments -PassThru
    $window = $null
    while ($sw.Elapsed.TotalSeconds -lt 30) {
        # Electron's visible window may belong to the main process or appear
        # once its renderer is ready; take the first tree process with one.
        foreach ($p in (Get-Tree $proc.Id)) {
            $p.Refresh()
            if ($p.MainWindowHandle -ne 0 -and $p.MainWindowTitle) { $window = $sw.Elapsed.TotalMilliseconds; break }
        }
        if ($window) { break }
        Start-Sleep -Milliseconds 5
    }
    Start-Sleep -Seconds $SettleSeconds
    $tree = @(Get-Tree $proc.Id)
    $cpuStart = ($tree | ForEach-Object { $_.TotalProcessorTime.TotalMilliseconds } | Measure-Object -Sum).Sum
    Start-Sleep -Seconds $IdleSeconds
    $tree = @(Get-Tree $proc.Id)
    foreach ($p in $tree) { $p.Refresh() }
    $cpuEnd = ($tree | ForEach-Object { $_.TotalProcessorTime.TotalMilliseconds } | Measure-Object -Sum).Sum
    $ws = ($tree | Measure-Object -Property WorkingSet64 -Sum).Sum
    $private = ($tree | Measure-Object -Property PrivateMemorySize64 -Sum).Sum
    $hostProcs = @($tree | Where-Object { $Companions -contains $_.ProcessName })
    $hostWs = ($hostProcs | Measure-Object -Property WorkingSet64 -Sum).Sum
    $hostPrivate = ($hostProcs | Measure-Object -Property PrivateMemorySize64 -Sum).Sum
    $result = [pscustomobject]@{
        WindowMs      = [math]::Round($window, 0)
        Processes     = $tree.Count
        WorkingMB     = [math]::Round($ws / 1MB, 1)
        PrivateMB     = [math]::Round($private / 1MB, 1)
        HostWorkingMB = [math]::Round($hostWs / 1MB, 1)
        HostPrivateMB = [math]::Round($hostPrivate / 1MB, 1)
        IdleCpuPct    = [math]::Round(($cpuEnd - $cpuStart) / ($IdleSeconds * 1000) * 100, 2)
    }
    # Close politely first so apps with databases (T3 Code) shut down cleanly,
    # then stop whatever is left of the tree.
    $tree = @(Get-Tree $proc.Id)
    foreach ($p in $tree) { if ($p.MainWindowHandle -ne 0) { [void]$p.CloseMainWindow() } }
    $deadline = (Get-Date).AddSeconds(8)
    while ((Get-Date) -lt $deadline -and @($tree | Where-Object { -not $_.HasExited }).Count -gt 0) {
        Start-Sleep -Milliseconds 200
        foreach ($p in $tree) { $p.Refresh() }
    }
    foreach ($p in ($tree | Where-Object { -not $_.HasExited })) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Seconds 2
    $result
}

$samples = 1..$Runs | ForEach-Object { Measure-Once }
$median = { param($xs) ($xs | Sort-Object)[[int][math]::Floor(($xs.Count - 1) / 2)] }
$size = (Get-Item $Exe).Length
$dir = Split-Path -Parent (Resolve-Path $Exe)
[pscustomobject]@{
    App          = $Label
    WindowMs     = & $median ($samples.WindowMs)
    Processes    = & $median ($samples.Processes)
    WorkingMB    = & $median ($samples.WorkingMB)
    PrivateMB    = & $median ($samples.PrivateMB)
    HostWorkingMB = & $median ($samples.HostWorkingMB)
    HostPrivateMB = & $median ($samples.HostPrivateMB)
    IdleCpuPct   = & $median ($samples.IdleCpuPct)
    ExeMB        = [math]::Round($size / 1MB, 1)
} | ConvertTo-Json -Compress
