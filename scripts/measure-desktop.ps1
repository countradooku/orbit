<#
.SYNOPSIS
  Measures a desktop app the way a user experiences it: time until its window
  is up, and the memory and idle CPU of its whole process tree.

.DESCRIPTION
  Electron apps (T3 Code) run a main process plus renderer, GPU and utility
  processes, so every number here sums the full tree, found through parent
  process ids. Idle CPU is measured after the app settles, as a percentage of
  one core.

.EXAMPLE
  ./scripts/measure-desktop.ps1 -Label "Orbit (GPUI Fast)" -Exe target/release/orbit.exe
#>
param(
    [Parameter(Mandatory)] [string] $Label,
    [Parameter(Mandatory)] [string] $Exe,
    [string[]] $Arguments = @(),
    [int] $SettleSeconds = 8,
    [int] $IdleSeconds = 10,
    [int] $Runs = 3
)

$ErrorActionPreference = 'Stop'

function Get-Tree([int] $RootId) {
    $all = Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId
    $ids = [System.Collections.Generic.HashSet[int]]::new()
    [void]$ids.Add($RootId)
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
    $result = [pscustomobject]@{
        WindowMs   = [math]::Round($window, 0)
        Processes  = $tree.Count
        WorkingMB  = [math]::Round($ws / 1MB, 1)
        PrivateMB  = [math]::Round($private / 1MB, 1)
        IdleCpuPct = [math]::Round(($cpuEnd - $cpuStart) / ($IdleSeconds * 1000) * 100, 2)
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
    IdleCpuPct   = & $median ($samples.IdleCpuPct)
    ExeMB        = [math]::Round($size / 1MB, 1)
} | ConvertTo-Json -Compress
