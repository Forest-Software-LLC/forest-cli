<#
.SYNOPSIS
Link + rojo-safe-install composition smoke.

With a live `rojo serve` attached: link a local working tree over a direct
dep, install (junction applied), force-reinstall (slot cleared as a LINK,
re-applied), CI-mode install (junction replaced by the registry dir), then
unlink (registry restored), asserting after every step that rojo survived
and the linked working tree was never mutated. Companion to rojo-bench.ps1
(which covers the linkless scenarios); run both when mount mutation or link
overlay code changes.

.EXAMPLE
.\scripts\rojo-link-smoke.ps1        # downloads rojo 7.7.0 if needed
#>
param(
    [string]$ForestExe = (Join-Path (Split-Path $PSScriptRoot -Parent) "target\release\forest.exe"),
    [string]$RojoExe = "",
    [string]$RojoVersion = "7.7.0",
    [string]$Root = (Join-Path $env:TEMP "forest-link-rojo-smoke"),
    [int]$Port = 35104
)
$ErrorActionPreference = "Continue"

if (-not (Test-Path $ForestExe)) {
    Write-Host "forest binary not found at $ForestExe (build with 'cargo build --release' or pass -ForestExe)"
    exit 1
}

# Reuse (or fetch) the same rojo the bench uses.
$BenchRoot = Join-Path $env:TEMP "forest-rojo-bench"
if (-not $RojoExe) {
    $RojoExe = Join-Path $BenchRoot "rojo-$RojoVersion.exe"
    if (-not (Test-Path $RojoExe)) {
        New-Item -ItemType Directory -Force $BenchRoot | Out-Null
        Write-Host "Downloading Rojo $RojoVersion..."
        $zip = Join-Path $BenchRoot "rojo.zip"
        Invoke-WebRequest -Uri "https://github.com/rojo-rbx/rojo/releases/download/v$RojoVersion/rojo-$RojoVersion-windows-x86_64.zip" -OutFile $zip
        Expand-Archive $zip -DestinationPath $BenchRoot -Force
        Move-Item (Join-Path $BenchRoot "rojo.exe") $RojoExe -Force
        Remove-Item $zip
    }
}
$fails = @()
function Assert([bool]$cond, [string]$what) {
    if ($cond) { Write-Host "  PASS $what" } else { Write-Host "  FAIL $what"; $script:fails += $what }
}

if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
$Project = Join-Path $Root "project"
$Dev = Join-Path $Root "knit-dev"
New-Item -ItemType Directory -Force $Project, (Join-Path $Project "Packages"), (Join-Path $Dev "src") | Out-Null

[System.IO.File]::WriteAllText((Join-Path $Project "forest.json"), '{ "name": "link-smoke", "platform": "roblox", "dependencies": {} }')
[System.IO.File]::WriteAllText((Join-Path $Project "default.project.json"), @"
{
  "name": "link-smoke",
  "servePort": $Port,
  "tree": {
    "`$className": "DataModel",
    "ReplicatedStorage": { "`$className": "ReplicatedStorage", "Packages": { "`$path": "Packages" } }
  }
}
"@)

# Local working tree posing as the direct dep (identity = author/name).
[System.IO.File]::WriteAllText((Join-Path $Dev "forest.json"), '{ "name": "knit", "author": "sleitnick", "version": "1.7.0", "platform": "roblox", "root": "src/init.luau", "dependencies": {} }')
[System.IO.File]::WriteAllText((Join-Path $Dev "src\init.luau"), "-- local dev knit`nreturn { DEV_MARKER = true }")
$devSentinel = Join-Path $Dev "src\SENTINEL.luau"
[System.IO.File]::WriteAllText($devSentinel, "return 'untouched'")

$env:FOREST_NO_UPDATE_CHECK = "1"
$rojoErr = Join-Path $Root "rojo.err.log"
$rojoOut = Join-Path $Root "rojo.out.log"
$rojo = Start-Process -FilePath $RojoExe -ArgumentList @("serve", "--port", "$Port") -WorkingDirectory $Project `
    -RedirectStandardOutput $rojoOut -RedirectStandardError $rojoErr -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
if ($rojo.HasExited) { Write-Host "FATAL: rojo died on start"; Get-Content $rojoOut, $rojoErr; exit 1 }

function RojoAlive([string]$step) {
    Start-Sleep -Seconds 3
    $rojo.Refresh()
    Assert (-not $rojo.HasExited) "rojo alive after $step"
    if ($rojo.HasExited) { Get-Content $rojoErr -Tail 30; exit 1 }
}

Push-Location $Project
$log = Join-Path $Root "forest.log"

Write-Host "== install direct dep from registry =="
& $ForestExe install sleitnick/knit *>> $log
Assert ($LASTEXITCODE -eq 0) "install sleitnick/knit exit 0"
RojoAlive "registry install"
$slot = Join-Path $Project "Packages\Knit"
Assert (Test-Path (Join-Path $slot "init.lua")) "registry Knit present"

Write-Host "== link local working tree =="
& $ForestExe link $Dev *>> $log
Assert ($LASTEXITCODE -eq 0) "forest link exit 0"
RojoAlive "link apply"
$item = Get-Item $slot -Force
$isLink = ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
Assert $isLink "Knit slot is a junction"
Assert ((Get-Content (Join-Path $slot "init.luau") -Raw -ErrorAction SilentlyContinue) -match "DEV_MARKER") "junction resolves to dev tree"

Write-Host "== install --force with active link =="
& $ForestExe install --force *>> $log
Assert ($LASTEXITCODE -eq 0) "install --force exit 0"
RojoAlive "force reinstall with link"
$item = Get-Item $slot -Force
Assert (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) "slot still a junction after --force"
Assert (Test-Path $devSentinel) "dev tree sentinel survives --force"

Write-Host "== plain install (idempotent, link kept) =="
& $ForestExe install *>> $log
Assert ($LASTEXITCODE -eq 0) "plain install exit 0"
RojoAlive "plain install with link"
$item = Get-Item $slot -Force
Assert (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) "slot still a junction after plain install"

Write-Host "== CI mode ignores link (junction replaced by registry dir) =="
$env:CI = "true"
& $ForestExe install *>> $log
Assert ($LASTEXITCODE -eq 0) "CI install exit 0"
RojoAlive "CI install (link ignored)"
$item = Get-Item $slot -Force
Assert (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "slot is a real dir under CI"
Assert (Test-Path $devSentinel) "dev tree sentinel survives CI replace"
Remove-Item Env:CI

Write-Host "== re-apply then unlink =="
& $ForestExe install *>> $log
RojoAlive "re-apply link"
& $ForestExe unlink sleitnick/knit *>> $log
Assert ($LASTEXITCODE -eq 0) "unlink exit 0"
RojoAlive "unlink + reinstall"
$item = Get-Item $slot -Force
Assert (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "slot restored to real dir"
Assert (Test-Path (Join-Path $slot "init.lua")) "registry Knit restored"
Assert (Test-Path $devSentinel) "dev tree sentinel survives unlink"
Assert ((Get-Content (Join-Path $Dev "src\init.luau") -Raw) -match "DEV_MARKER") "dev root module untouched"

Pop-Location
$rojo.Refresh()
$survived = -not $rojo.HasExited
try { Stop-Process -Id $rojo.Id -Force -ErrorAction Stop } catch {}
$panics = @(Select-String -Path $rojoErr -Pattern "panic" -SimpleMatch -ErrorAction SilentlyContinue).Count
Assert $survived "rojo survived to the end"
Assert ($panics -eq 0) "no panics in rojo stderr"

Write-Host ""
if ($fails.Count -eq 0) { Write-Host "=== ALL PASS ==="; exit 0 }
else { Write-Host ("=== {0} FAILURES ===" -f $fails.Count); $fails | ForEach-Object { Write-Host " - $_" }; exit 1 }
