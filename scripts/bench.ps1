<#
.SYNOPSIS
  Benchmark harness for `forest install` performance (runs against prod).

.DESCRIPTION
  Two modes:

  1) Fixture setup (once). Creates a shared fixture project by installing a
     handful of real registry packages, then snapshots forest.json +
     forest-lock.json so every benchmarked binary installs the IDENTICAL
     resolved tree (removes resolution nondeterminism from comparisons):

       .\scripts\bench.ps1 -Exe target\release\forest.exe -SetupFixture

  2) Benchmark a binary across scenarios (N timed runs each, after one
     untimed warmup; median/min/max reported, rows appended to results.csv):

       .\scripts\bench.ps1 -Exe path\to\forest-baseline.exe -Label baseline
       .\scripts\bench.ps1 -Exe target\release\forest.exe   -Label new

  Scenarios:
    cold      forest.json only - no lockfile, no Packages, EMPTY tarball cache
              (full resolve + download)
    reinstall forest.json + lockfile, Packages already populated, re-run
              `forest install` in place (the "nothing changed" case)
    warmcache fresh project dir with lockfile, no Packages, global tarball
              cache pre-warmed (cross-project install)
    resolve   no lockfile (forces re-resolution), Packages populated, cache
              warm (isolates resolution cost on binaries with reconciliation)

  Notes: FOREST_NO_UPDATE_CHECK=1 is set for all runs. FOREST_CACHE_DIR is
  pointed at per-scenario dirs (ignored by binaries without cache support).
  This file must stay pure ASCII: Windows PowerShell 5.1 parses BOM-less
  scripts as ANSI, where stray UTF-8 bytes turn into smart quotes.
#>
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string]$Label = "run",
    [switch]$SetupFixture,
    [string[]]$Scenarios = @("cold", "reinstall", "warmcache", "resolve"),
    [int]$N = 5,
    [string]$BenchRoot = "$env:TEMP\forest-bench"
)

$ErrorActionPreference = "Stop"
$Exe = (Resolve-Path $Exe).Path
$FixtureDir = Join-Path $BenchRoot "fixture"
$ResultsCsv = Join-Path $BenchRoot "results.csv"

# Real, MIT-licensed packages on the prod registry (Wally mirror). knit brings
# transitive deps, exercising nesting/hoisting/pointer generation.
$FixturePackages = @("sleitnick/knit", "sleitnick/signal", "sleitnick/trove", "evaera/promise", "roblox/roact")

$env:FOREST_NO_UPDATE_CHECK = "1"

function New-CleanDir([string]$Path) {
    if (Test-Path $Path) { Remove-Item -Recurse -Force $Path }
    New-Item -ItemType Directory -Force $Path | Out-Null
    return $Path
}

function Invoke-Forest([string[]]$ForestArgs, [string]$Cwd) {
    Push-Location $Cwd
    try {
        & $Exe @ForestArgs *> $null
        return $LASTEXITCODE
    } finally {
        Pop-Location
    }
}

function Get-Median([double[]]$Values) {
    $sorted = $Values | Sort-Object
    $c = $sorted.Count
    if ($c -eq 0) { return 0 }
    return ($sorted[[math]::Floor(($c - 1) / 2)] + $sorted[[math]::Ceiling(($c - 1) / 2)]) / 2
}

# ---------------------------------------------------------------- fixture ---
if ($SetupFixture) {
    Write-Host "Setting up fixture with $Exe against the registry..."
    $work = New-CleanDir (Join-Path $BenchRoot "fixture-work")

    $code = Invoke-Forest @("init", "-p", "roblox") $work
    if ($code -ne 0) { throw "forest init failed (exit $code)" }

    $failed = @()
    foreach ($pkg in $FixturePackages) {
        Write-Host "  installing $pkg ..."
        $code = Invoke-Forest @("install", $pkg) $work
        $manifest = Get-Content (Join-Path $work "forest.json") -Raw
        if ($code -ne 0 -or $manifest -notmatch [regex]::Escape($pkg)) {
            $failed += $pkg
            Write-Warning "  $pkg failed (exit $code) - swap it for another prod package in FixturePackages"
        }
    }
    if ($failed.Count -eq $FixturePackages.Count) { throw "No fixture package installed successfully." }

    New-CleanDir $FixtureDir | Out-Null
    Copy-Item (Join-Path $work "forest.json") $FixtureDir
    Copy-Item (Join-Path $work "forest-lock.json") $FixtureDir

    $lock = Get-Content (Join-Path $FixtureDir "forest-lock.json") -Raw | ConvertFrom-Json
    $pkgCount = 0
    foreach ($p in $lock.packages.PSObject.Properties) { $pkgCount += @($p.Value).Count }
    Write-Host "Fixture ready: $($FixturePackages.Count - $failed.Count) direct deps, $pkgCount package versions in lockfile."
    if ($failed.Count -gt 0) { Write-Warning ("Not in fixture: " + ($failed -join ", ")) }
    exit 0
}

# ------------------------------------------------------------- benchmarks ---
if (-not (Test-Path (Join-Path $FixtureDir "forest-lock.json"))) {
    throw "Fixture missing. Run: .\scripts\bench.ps1 -Exe your-forest.exe -SetupFixture"
}

function Seed-Project([string]$Dir, [bool]$WithLockfile) {
    Copy-Item (Join-Path $FixtureDir "forest.json") $Dir
    if ($WithLockfile) { Copy-Item (Join-Path $FixtureDir "forest-lock.json") $Dir }
}

function Time-Install([string]$Cwd) {
    $t = Measure-Command {
        Push-Location $Cwd
        try { & $Exe install *> $null } finally { Pop-Location }
    }
    if ($LASTEXITCODE -ne 0) { Write-Warning "install exited $LASTEXITCODE in $Cwd" }
    return @{ Ms = [math]::Round($t.TotalMilliseconds, 1); Exit = $LASTEXITCODE }
}

if (-not (Test-Path $ResultsCsv)) {
    "timestamp,label,scenario,iteration,ms,exit" | Out-File -Encoding utf8 $ResultsCsv
}

$summary = @()
foreach ($scenario in $Scenarios) {
    $runRoot = New-CleanDir (Join-Path $BenchRoot "runs\$Label-$scenario")
    $cacheDir = New-CleanDir (Join-Path $BenchRoot "cache\$Label-$scenario")
    $env:FOREST_CACHE_DIR = $cacheDir

    # Per-scenario preparation + per-iteration body.
    switch ($scenario) {
        "cold" {
            $iter = {
                param($i)
                $d = New-CleanDir (Join-Path $runRoot "i$i")
                New-CleanDir $cacheDir | Out-Null   # cold cache every run
                Seed-Project $d $false
                Time-Install $d
            }
        }
        "reinstall" {
            $proj = New-CleanDir (Join-Path $runRoot "proj")
            Seed-Project $proj $true
            Invoke-Forest @("install") $proj | Out-Null   # populate Packages (untimed)
            $iter = { param($i) Time-Install $proj }
        }
        "warmcache" {
            $warm = New-CleanDir (Join-Path $runRoot "warm")
            Seed-Project $warm $true
            Invoke-Forest @("install") $warm | Out-Null   # warm the tarball cache (untimed)
            $iter = {
                param($i)
                $d = New-CleanDir (Join-Path $runRoot "i$i")
                Seed-Project $d $true
                Time-Install $d
            }
        }
        "resolve" {
            $proj = New-CleanDir (Join-Path $runRoot "proj")
            Seed-Project $proj $true
            Invoke-Forest @("install") $proj | Out-Null   # populate Packages + cache (untimed)
            $iter = {
                param($i)
                Remove-Item (Join-Path $proj "forest-lock.json") -Force -ErrorAction SilentlyContinue
                Time-Install $proj
            }
        }
        default { throw "Unknown scenario: $scenario" }
    }

    Write-Host ""
    Write-Host "=== $Label / $scenario (warmup + $N runs) ==="
    & $iter 0 | Out-Null   # untimed warmup (also CDN/DNS/TLS/AV warm)

    $times = @()
    for ($i = 1; $i -le $N; $i++) {
        $r = & $iter $i
        $times += $r.Ms
        "{0},{1},{2},{3},{4},{5}" -f (Get-Date -Format o), $Label, $scenario, $i, $r.Ms, $r.Exit |
            Add-Content -Encoding utf8 $ResultsCsv
        Write-Host ("  run {0}: {1,8:N1} ms" -f $i, $r.Ms)
    }

    $med = Get-Median $times
    $row = [pscustomobject]@{
        Scenario = $scenario
        MedianMs = [math]::Round($med, 1)
        MinMs    = [math]::Round(($times | Measure-Object -Minimum).Minimum, 1)
        MaxMs    = [math]::Round(($times | Measure-Object -Maximum).Maximum, 1)
    }
    $summary += $row
    Write-Host ("  median {0:N1} ms  (min {1:N1} / max {2:N1})" -f $row.MedianMs, $row.MinMs, $row.MaxMs)
}

Remove-Item Env:\FOREST_CACHE_DIR -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "=== Summary: $Label ==="
$summary | Format-Table -AutoSize
Write-Host "Rows appended to $ResultsCsv"
