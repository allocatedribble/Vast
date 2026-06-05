<#
  rebuild-editor.ps1 - Close the Unreal Editor, rebuild LogiUEEditor (Rust + C++), relaunch at a level.

  Usage:
    powershell -ExecutionPolicy Bypass -File .\rebuild-editor.ps1
    powershell -ExecutionPolicy Bypass -File .\rebuild-editor.ps1 -Map /Game/NewMap
    powershell -ExecutionPolicy Bypass -File .\rebuild-editor.ps1 -NoLaunch   # build only
    powershell -ExecutionPolicy Bypass -File .\rebuild-editor.ps1 -SkipSave    # don't auto-save
    (levels present: /Game/Logi, /Game/NewMap, /Game/NewWorld)

  It AUTO-SAVES the editor first (control_editor save_all via the in-editor MCP bridge on :3000),
  then force-closes it - Live Coding holds the DLLs open, so the editor process must fully terminate
  before the linker can run. Pass -SkipSave to skip the save. If the bridge can't be reached the
  script aborts (rather than force-closing and losing work).
#>
[CmdletBinding()]
param(
    [string]$Map    = "/Game/Logi",        # level to open on launch (-Map /Game/NewMap for the other one)
    [string]$Config = "Development",
    [switch]$NoLaunch,
    [switch]$SkipSave                      # skip the auto-save (force-close even with unsaved work)
)
$ErrorActionPreference = "Stop"
$sw = [Diagnostics.Stopwatch]::StartNew()

# ---- Paths -------------------------------------------------------------------
$Project  = "C:\Users\premi\work\LogiUE\LogiUE.uproject"
$RustDir  = "C:\Users\premi\work\LogiUE\Rust"
# The EDITOR target dir (matches Build.cs RustOutDir = target/ue-editor/release): the C++ links its import
# lib + runs its DLL from here. Build + stage THIS, not the default target/release.
$RustOut  = Join-Path $RustDir "target\ue-editor\release"
$Binaries = "C:\Users\premi\work\LogiUE\Plugins\LogiBevy\Binaries\Win64"
$Engine   = "C:\Program Files\Epic Games\UE_5.8\Engine"
$Editor   = Join-Path $Engine "Binaries\Win64\UnrealEditor.exe"
$BuildBat = Join-Path $Engine "Build\BatchFiles\Build.bat"

function Step([int]$n, [string]$msg) { Write-Host "`n==> [$n] $msg" -ForegroundColor Cyan }
function Note([string]$msg)          { Write-Host "    $msg" -ForegroundColor DarkGray }

# Save all dirty packages (levels + assets) through the in-editor MCP bridge. Returns the saved count.
# The editor process and the bridge live together, so whenever there's work to save the bridge is up.
function Save-EditorViaBridge {
    param([string]$Url = "http://127.0.0.1:3000/mcp", [int]$TimeoutSec = 60)
    $acc = "application/json, text/event-stream"
    $r1 = Invoke-WebRequest -Uri $Url -Method Post -ContentType "application/json" -Headers @{ Accept = $acc } `
            -Body '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"rebuild-editor","version":"1"}}}' `
            -UseBasicParsing -TimeoutSec 10
    $sid = ($r1.Headers["Mcp-Session-Id"] | Select-Object -First 1)
    if (-not $sid) { throw "MCP bridge returned no session id" }
    $h = @{ Accept = $acc; "Mcp-Session-Id" = $sid }
    Invoke-WebRequest -Uri $Url -Method Post -ContentType "application/json" -Headers $h `
        -Body '{"jsonrpc":"2.0","method":"notifications/initialized"}' -UseBasicParsing -TimeoutSec 10 | Out-Null
    $r3 = Invoke-WebRequest -Uri $Url -Method Post -ContentType "application/json" -Headers $h `
        -Body '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"control_editor","arguments":{"action":"save_all"}}}' `
        -UseBasicParsing -TimeoutSec $TimeoutSec
    $m = [regex]::Match($r3.Content, '(?m)^data:\s*(.+)$')   # streamable-HTTP reply is SSE; JSON on the data: line
    if (-not $m.Success) { throw "unexpected save_all response: $($r3.Content)" }
    $res = ($m.Groups[1].Value | ConvertFrom-Json).result
    if ($res.isError) { throw ($res.content[0].text) }
    if ($res.structuredContent.failedCount -gt 0) { throw "failed to save: $($res.structuredContent.failedPackages -join ', ')" }
    return [int]$res.structuredContent.savedCount
}

# Make cargo visible both to this session AND to UBT's cargo pre-step (Build.cs shells out to `cargo`;
# if it isn't on PATH it silently falls back to the stale prebuilt Rust artifact).
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw "cargo not found on PATH (looked in ~/.cargo/bin)." }

# ---- 1) Save open work, then close the editor and wait for it to release the DLLs ----
Step 1 "Saving + closing UnrealEditor..."
$running = Get-Process -Name UnrealEditor* -ErrorAction SilentlyContinue
if ($running) {
    if (-not $SkipSave) {
        Note "saving all dirty packages via MCP bridge..."
        try { $saved = Save-EditorViaBridge; Note "saved $saved package(s)." }
        catch { throw "Auto-save failed: $($_.Exception.Message)`n     Save + close the editor manually, or re-run with -SkipSave to force-close." }
    } else { Note "-SkipSave: not saving." }
    $running | ForEach-Object { $_.CloseMainWindow() | Out-Null }   # ask nicely first
    Start-Sleep -Seconds 3
    Get-Process -Name UnrealEditor* -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Process -Name UnrealEditor* -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
    }
    if (Get-Process -Name UnrealEditor* -ErrorAction SilentlyContinue) { throw "Editor did not terminate; aborting." }
    Note "closed."
} else { Note "already closed." }

# ---- 2) Build the Rust cdylibs ----------------------------------------------
# The editor target needs logi_ffi built WITH the editor_inspector feature (matches Build.cs).
Step 2 "Building Rust (logi_ffi +editor_inspector, logi_terrain)..."
# Build into the EDITOR target dir (target/ue-editor) — the exact dir Build.cs links the import lib from
# and stages the runtime DLL. UBT's own cargo pre-step is SKIPPED on cached incremental builds, which left
# a stale import lib missing newly-added exports -> LNK2019; building it here keeps the link lib fresh.
& cargo build --release --manifest-path "$RustDir\Cargo.toml" -p logi_ffi --features editor_inspector --target-dir "$RustDir\target\ue-editor"
if ($LASTEXITCODE -ne 0) { throw "cargo build (logi_ffi) failed." }
& cargo build --release --manifest-path "$RustDir\Cargo.toml" -p logi_terrain_ffi --target-dir "$RustDir\target\ue-editor"
if ($LASTEXITCODE -ne 0) { throw "cargo build (logi_terrain_ffi) failed." }
# Stage fresh DLLs into the editor's binary dir (UBT's RuntimeDependencies copy is skipped on
# incremental builds, which is exactly how the Rust DLL went stale during our session).
foreach ($f in @("logi_ffi.dll", "logi_terrain.dll")) {
    Copy-Item -Force (Join-Path $RustOut $f) (Join-Path $Binaries $f)
}
Note "rust built + staged."

# ---- 3) Build the C++ editor target -----------------------------------------
Step 3 "Building LogiUEEditor ($Config)..."
& $BuildBat LogiUEEditor Win64 $Config -Project="$Project" -WaitMutex -NoHotReload
if ($LASTEXITCODE -ne 0) { throw "C++ build failed (exit $LASTEXITCODE) - editor NOT relaunched." }
Note "C++ build succeeded."

# ---- Verify the artifacts are fresh -----------------------------------------
Step 4 "Artifacts:"
Get-ChildItem (Join-Path $Binaries "logi_terrain.dll"),
              (Join-Path $Binaries "logi_ffi.dll"),
              (Join-Path $Binaries "UnrealEditor-LogiBevyRuntime.dll") -ErrorAction SilentlyContinue |
    Select-Object @{N='DLL';E={$_.Name}}, LastWriteTime | Format-Table -AutoSize | Out-String | Write-Host

# ---- 5) Relaunch the editor at the level ------------------------------------
if ($NoLaunch) {
    Note "build only (-NoLaunch); editor left closed."
} else {
    Step 5 "Launching editor at $Map ..."
    Start-Process -FilePath $Editor -ArgumentList @("`"$Project`"", $Map)
    Note "launched."
}

$sw.Stop()
Write-Host "`nDone in $([math]::Round($sw.Elapsed.TotalSeconds,1))s." -ForegroundColor Green
