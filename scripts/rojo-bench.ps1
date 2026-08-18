<#
.SYNOPSIS
Rojo crash bench: proves forest install/remove never crash a live `rojo serve`.

Runs a real rojo dev server (with an emulated Studio client subscribed over
its WebSocket API) against a scratch project, then drives forest through
install / incremental add / remove / force-reinstall / update / rapid
zero-gap cycles, checking after each command that rojo is still alive and
panic-free. Rojo 7.7.0's change processor unwraps canonicalize() on every
watcher event path, so any transient state where an event's path no longer
resolves kills the server - this bench is the regression net for forest's
rename-based mount mutation strategy (see src/roblox/install.rs).

.EXAMPLE
.\scripts\rojo-bench.ps1                       # downloads rojo 7.7.0, uses target\release\forest.exe
.\scripts\rojo-bench.ps1 -RojoExe C:\tools\rojo.exe -Label pr-check
#>
param(
    [string]$ForestExe = (Join-Path (Split-Path $PSScriptRoot -Parent) "target\release\forest.exe"),
    [string]$RojoExe = "",
    [string]$RojoVersion = "7.7.0",
    [string]$BenchRoot = (Join-Path $env:TEMP "forest-rojo-bench"),
    [string]$Label = "run",
    [int]$Port = 35103
)
$ErrorActionPreference = "Continue"

if (-not (Test-Path $ForestExe)) {
    Write-Host "forest binary not found at $ForestExe (build with 'cargo build --release' or pass -ForestExe)"
    exit 1
}

New-Item -ItemType Directory -Force $BenchRoot | Out-Null

# Fetch rojo if not provided
if (-not $RojoExe) {
    $RojoExe = Join-Path $BenchRoot "rojo-$RojoVersion.exe"
    if (-not (Test-Path $RojoExe)) {
        Write-Host "Downloading Rojo $RojoVersion..."
        $zip = Join-Path $BenchRoot "rojo.zip"
        Invoke-WebRequest -Uri "https://github.com/rojo-rbx/rojo/releases/download/v$RojoVersion/rojo-$RojoVersion-windows-x86_64.zip" -OutFile $zip
        Expand-Archive $zip -DestinationPath $BenchRoot -Force
        Move-Item (Join-Path $BenchRoot "rojo.exe") $RojoExe -Force
        Remove-Item $zip
    }
}
& $RojoExe --version | Write-Host

$Project = Join-Path $BenchRoot "project"
$Logs = Join-Path $BenchRoot "logs-$Label"
$env:FOREST_NO_UPDATE_CHECK = "1"

if (Test-Path $Project) { Remove-Item -Recurse -Force $Project }
if (Test-Path $Logs) { Remove-Item -Recurse -Force $Logs }
New-Item -ItemType Directory -Force $Project | Out-Null
New-Item -ItemType Directory -Force $Logs | Out-Null
New-Item -ItemType Directory -Force (Join-Path $Project "Packages") | Out-Null

# WriteAllText: UTF-8 without BOM (Out-File's BOM breaks rojo's JSON parser)
[System.IO.File]::WriteAllText((Join-Path $Project "forest.json"), @'
{
  "name": "rojo-bench",
  "platform": "roblox",
  "dependencies": {}
}
'@)

# servePort lets forest's rojo probe find this server on the custom port.
[System.IO.File]::WriteAllText((Join-Path $Project "default.project.json"), @"
{
  "name": "rojo-bench",
  "servePort": $Port,
  "tree": {
    "`$className": "DataModel",
    "ReplicatedStorage": {
      "`$className": "ReplicatedStorage",
      "Packages": { "`$path": "Packages" }
    }
  }
}
"@)

# Start rojo serve
$rojoOut = Join-Path $Logs "rojo.out.log"
$rojoErr = Join-Path $Logs "rojo.err.log"
$rojo = Start-Process -FilePath $RojoExe `
    -ArgumentList @("serve", "--port", "$Port") `
    -WorkingDirectory $Project `
    -RedirectStandardOutput $rojoOut -RedirectStandardError $rojoErr `
    -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
if ($rojo.HasExited) {
    Write-Host "FATAL: rojo serve exited immediately:"
    Get-Content $rojoOut, $rojoErr
    exit 1
}

# Start the fake Studio subscriber
$subLog = Join-Path $Logs "subscriber.log"
$sub = Start-Process -FilePath "powershell.exe" `
    -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", (Join-Path $PSScriptRoot "rojo-subscriber.ps1"), "-Port", "$Port", "-LogPath", $subLog) `
    -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2

$results = @()
function Invoke-Scenario {
    param([string]$Name, [object[]]$Commands)
    $log = Join-Path $Logs "$Name.log"
    $flat = ($Commands | ForEach-Object { "forest $($_ -join ' ')" }) -join "; "
    Write-Host ">> $Name : $flat"
    Push-Location $Project
    $forestExit = 0
    foreach ($ForestArgs in $Commands) {
        & $ForestExe @ForestArgs *>> $log
        if ($LASTEXITCODE -ne 0) { $forestExit = $LASTEXITCODE }
    }
    Pop-Location
    # Let the watcher chew on the changes
    Start-Sleep -Seconds 3
    $rojo.Refresh()
    $alive = -not $rojo.HasExited
    $panics = @(Select-String -Path $rojoErr -Pattern "panic", "PANIC", "Rojo crashed" -SimpleMatch -ErrorAction SilentlyContinue).Count
    $status = if (-not $alive) { "ROJO-DEAD" } elseif ($panics -gt 0) { "ROJO-PANIC-LOGGED" } else { "ok" }
    $script:results += [pscustomobject]@{ Scenario = $Name; ForestExit = $forestExit; Rojo = $status }
    Write-Host "   forest exit=$forestExit rojo=$status"
    if (-not $alive) {
        Write-Host "--- rojo stderr tail ---"
        Get-Content $rojoErr -Tail 40
    }
    return $alive
}

# Scenarios. Stop early if rojo dies (later results would be meaningless).
$scenarios = @(
    @{ Name = "01-cold-install-knit"; Cmds = @(,@("install", "sleitnick/knit")) },
    @{ Name = "02-add-more"; Cmds = @(,@("install", "evaera/promise")) },
    @{ Name = "03-add-trove"; Cmds = @(,@("install", "sleitnick/trove")) },
    @{ Name = "04-noop-install"; Cmds = @(,@("install")) },
    @{ Name = "05-remove-leaf"; Cmds = @(,@("remove", "sleitnick/trove")) },
    @{ Name = "06-remove-tree-knit"; Cmds = @(,@("remove", "sleitnick/knit")) },
    @{ Name = "07-reinstall-knit"; Cmds = @(,@("install", "sleitnick/knit")) },
    @{ Name = "08-force-reinstall"; Cmds = @(,@("install", "--force")) },
    @{ Name = "09-force-again"; Cmds = @(,@("install", "--force")) },
    @{ Name = "10-remove-promise-hoist-shift"; Cmds = @(,@("remove", "evaera/promise")) },
    @{ Name = "11-update"; Cmds = @(,@("update")) },
    @{ Name = "12-rapid-cycles"; Cmds = @(
        @("remove", "sleitnick/knit"), @("install", "sleitnick/knit"),
        @("remove", "sleitnick/knit"), @("install", "sleitnick/knit"),
        @("install", "--force"), @("remove", "sleitnick/knit"), @("install", "sleitnick/knit")
    ) }
)
foreach ($s in $scenarios) {
    if (-not (Invoke-Scenario -Name $s.Name -Commands $s.Cmds)) { break }
}

# Teardown
try { Stop-Process -Id $sub.Id -Force -ErrorAction Stop } catch {}
$rojo.Refresh()
$rojoSurvived = -not $rojo.HasExited
try { Stop-Process -Id $rojo.Id -Force -ErrorAction Stop } catch {}

Write-Host ""
Write-Host "=== RESULTS ($Label) ==="
$results | Format-Table -AutoSize | Out-String | Write-Host
Write-Host ("rojo survived to the end: {0}" -f $rojoSurvived)
if (-not $rojoSurvived) {
    Write-Host "rojo stderr:"
    Get-Content $rojoErr -ErrorAction SilentlyContinue | Select-Object -Last 60
    exit 1
}
