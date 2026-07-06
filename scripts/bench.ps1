#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Reproducible micro-benchmarks for insane-cli (Windows / PowerShell).

.DESCRIPTION
    Measures:
      1. Startup latency of `insane --help` and `insane config path`, N runs,
         reports median/min/max (SPEC §9 target: < 50ms for --help).
      2. Peak working-set memory of a single `insane --help` invocation.
    Requires a release build: run `cargo build --release` first.

.EXAMPLE
    pwsh ./scripts/bench.ps1
    pwsh ./scripts/bench.ps1 -N 50
#>
param(
    [int]$N = 20,
    [string]$BinPath = ""
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $BinPath) {
    $BinPath = Join-Path $repoRoot "target\release\insane.exe"
}

if (-not (Test-Path $BinPath)) {
    Write-Error "Release binary not found at $BinPath -- run 'cargo build --release' first."
    exit 1
}

function Measure-Startup {
    # NOTE: deliberately not named `$Args` -- that's PowerShell's automatic
    # variable for unbound arguments, and shadowing it here silently breaks
    # argument splatting below.
    param([string[]]$CmdArgs, [int]$Runs)

    $times = @()
    for ($i = 0; $i -lt $Runs; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $BinPath @CmdArgs | Out-Null
        $sw.Stop()
        $times += $sw.Elapsed.TotalMilliseconds
    }
    $sorted = $times | Sort-Object
    [PSCustomObject]@{
        Min    = [Math]::Round(($sorted[0]), 2)
        Median = [Math]::Round(($sorted[[Math]::Floor($Runs / 2)]), 2)
        Max    = [Math]::Round(($sorted[$Runs - 1]), 2)
        Runs   = $Runs
    }
}

Write-Host "insane-cli benchmark -- $(Get-Date -Format o)"
Write-Host "Binary: $BinPath"
Write-Host "Runs per measurement: $N"
Write-Host ""

Write-Host "--- Startup latency: 'insane --help' ---"
$helpStats = Measure-Startup -CmdArgs @("--help") -Runs $N
$helpStats | Format-Table | Out-String | Write-Host

Write-Host "--- Startup latency: 'insane config path' ---"
$configStats = Measure-Startup -CmdArgs @("config", "path") -Runs $N
$configStats | Format-Table | Out-String | Write-Host

Write-Host "--- Peak working-set memory (single 'insane --help' run) ---"
$proc = Start-Process -FilePath $BinPath -ArgumentList "--help" -PassThru -WindowStyle Hidden -RedirectStandardOutput "NUL"
$peakKB = 0
try {
    while (-not $proc.HasExited) {
        try {
            $proc.Refresh()
            if ($proc.PeakWorkingSet64 -gt $peakKB) { $peakKB = $proc.PeakWorkingSet64 }
        } catch {}
        Start-Sleep -Milliseconds 5
    }
} finally {
    if (-not $proc.HasExited) { $proc.Kill() }
}
$proc.Refresh()
$peakBytes = [Math]::Max($peakKB, $proc.PeakWorkingSet64)
Write-Host ("Peak working set: {0:N0} KB ({1:N2} MB)" -f ($peakBytes / 1KB), ($peakBytes / 1MB))

Write-Host ""
Write-Host "--- Summary (paste into docs/BENCHMARKS.md) ---"
Write-Host "help_median_ms=$($helpStats.Median) help_min_ms=$($helpStats.Min) help_max_ms=$($helpStats.Max)"
Write-Host "config_path_median_ms=$($configStats.Median) config_path_min_ms=$($configStats.Min) config_path_max_ms=$($configStats.Max)"
Write-Host "peak_working_set_kb=$([Math]::Round($peakBytes / 1KB, 0))"
