param(
    [string]$GamePath = "",
    [string]$ExePath = "",
    [switch]$SeriesX,
    [switch]$Windowed,
    [switch]$Fullscreen,
    [switch]$DirectStorageHalf
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $candidates = @(
        (Join-Path $RepoRoot "target\release\victory.exe"),
        (Join-Path $RepoRoot "target\diag\victory.exe")
    )
    $ExePath = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $ExePath) {
        throw "Could not find victory.exe under target\release or target\diag. Build first: cargo build --locked --release -p victory --bins"
    }
}

$ConsoleProfile = if ($SeriesX) { "series-x" } else { "series-s" }

# RTX 20-series defaults (all core optimizations + 6 GB VRAM caps are also built-in defaults)
if (-not $env:VICTORY_VIEW_THREADS) { $env:VICTORY_VIEW_THREADS = "4" }
if (-not $env:VICTORY_AMBIENT) { $env:VICTORY_AMBIENT = "0.8" }
if (-not $env:VICTORY_AMBIENT_AIR) { $env:VICTORY_AMBIENT_AIR = "1.5" }
if (-not $env:VICTORY_CARD_BUFFERS_MIN_KB) { $env:VICTORY_CARD_BUFFERS_MIN_KB = "64" }
if (-not $env:VICTORY_MAP_THREADS) { $env:VICTORY_MAP_THREADS = "1" }
if (-not $env:VICTORY_CARD_AWAKE) { $env:VICTORY_CARD_AWAKE = "1" }

if ($DirectStorageHalf) {
    $env:VICTORY_DSTORAGE_HALF = "1"
}

if ($Fullscreen) {
    $env:VICTORY_FULLSCREEN = "1"
} elseif ($Windowed) {
    $env:VICTORY_FULLSCREEN = "0"
}

$argsList = @("--console", $ConsoleProfile, "--execute")
if (-not [string]::IsNullOrWhiteSpace($GamePath)) {
    $argsList = @($GamePath) + $argsList
}

Write-Host "[VIctory RTX20] Exe: $ExePath"
Write-Host "[VIctory RTX20] Profile: $ConsoleProfile | VICTORY_VIEW_THREADS=$($env:VICTORY_VIEW_THREADS)"
& $ExePath @argsList

