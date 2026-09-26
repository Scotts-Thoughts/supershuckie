<#
.SYNOPSIS
  Run the Azahar feasibility spike's measurement matrix on one ROM and collect the numbers.

.DESCRIPTION
  For the given ROM (a decrypted .cci/.cxi/.3dsx), runs azahar_spike.exe for:
    1. OpenGL renderer (hidden window), New 3DS, JIT: fps, state size/time, FCRAM hashes, round trip
    2. the same run again in a fresh process: the hashes must match run 1 (cross-process determinism)
    3. OpenGL, interpreter (--nojit): hashes must match the JIT runs too
    4. Old 3DS mode: state size with 128 MB FCRAM
    5. software renderer (fewer frames, it is slow): fps, hashes vs OpenGL
    6. cross-process save-state load: the state written by run 1 is loaded in a new process, run
       the round-trip length, and the hash must equal run 1's post-save hash
  Writes every run's output to <OutDir>/<name>.txt and a summary to <OutDir>/summary.txt.
#>
param(
    [Parameter(Mandatory = $true)][string]$Rom,
    [string]$Exe = "",
    [string]$OutDir = "azahar-spike-results",
    [int]$Frames = 1800,
    [int]$Warmup = 300,
    [int]$Roundtrip = 600,
    [switch]$SkipSoftware,
    [int]$SoftwareFrames = 600
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (-not $Exe) {
    $found = Get-ChildItem -Path "build/azahar" -Recurse -Filter azahar_spike.exe | Select-Object -First 1
    if (-not $found) { throw "azahar_spike.exe not found under build/azahar; run scripts/build-azahar-spike.ps1" }
    $Exe = $found.FullName
}
New-Item -ItemType Directory -Force $OutDir | Out-Null
$userDir = Join-Path (Resolve-Path $OutDir).Path "user"
$state = Join-Path (Resolve-Path $OutDir).Path "final.cst"

function Run([string]$name, [string[]]$extra) {
    $args = @($Rom, "--frames", $Frames, "--warmup", $Warmup, "--user-dir", $userDir, "--hash-every", 600) + $extra
    Write-Host "== $name : azahar_spike $($args -join ' ')"
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $out = & $Exe @args 2>&1 | ForEach-Object { "$_" }
    $sw.Stop()
    $out += "wall_ms=$($sw.ElapsedMilliseconds)"
    $out | Out-File -Encoding utf8 (Join-Path $OutDir "$name.txt")
    $out | Where-Object { $_ -match "^(load_status|timed_frames|states=|hash |roundtrip|final_state|gl_|run_loop_status|error|dump_|load_file|wall_ms)" } | ForEach-Object { Write-Host "   $_" }
    return $out
}

$summary = @()
$gl = @("--renderer", "opengl")
$r1 = Run "1-opengl-jit" ($gl + @("--script", "--state-every", 600, "--roundtrip", $Roundtrip, "--save-file", $state, "--dump", (Join-Path $OutDir "frame-gl")))
$r2 = Run "2-opengl-jit-again" ($gl + @("--script", "--roundtrip", $Roundtrip))
$r3 = Run "3-opengl-nojit" ($gl + @("--script"))
$r4 = Run "4-opengl-old3ds" ($gl + @("--script", "--state-every", 600, "--old3ds"))
if (-not $SkipSoftware) {
    $r5 = Run "5-software-jit" @("--script", "--state-every", 600, "--dump", (Join-Path $OutDir "frame-sw"), "--frames", $SoftwareFrames)
}
$r6 = Run "6-load-state-new-process" ($gl + @("--script", "--load-file", $state, "--frames", $Roundtrip, "--warmup", 0, "--hash-every", $Roundtrip))

function Hashes($out) { $out | Where-Object { $_ -match "^hash frame=" } }
$summary += "run1 hashes: " + ((Hashes $r1) -join " | ")
$summary += "run2 hashes: " + ((Hashes $r2) -join " | ")
$summary += "nojit hashes: " + ((Hashes $r3) -join " | ")
if ($r5) { $summary += "software hashes: " + ((Hashes $r5) -join " | ") }
$summary += "cross-process determinism (run1 == run2): " + $(if (((Hashes $r1) -join "") -eq ((Hashes $r2) -join "")) { "MATCH" } else { "MISMATCH" })
$summary += "jit == interpreter: " + $(if (((Hashes $r1) -join "") -eq ((Hashes $r3) -join "")) { "MATCH" } else { "MISMATCH" })
if ($r5) { $summary += "opengl == software FCRAM (GPU-written buffers may differ by design): " + $(if ((((Hashes $r1) | ForEach-Object { ($_ -split ' ')[2] }) -join "") -eq (((Hashes $r5) | ForEach-Object { ($_ -split ' ')[2] }) -join "")) { "MATCH" } else { "MISMATCH" }) }
$summary += "run1 roundtrip: " + ($r1 | Where-Object { $_ -match "^roundtrip" })
$summary += "run1 state: " + ($r1 | Where-Object { $_ -match "^states=" })
$summary += "old3ds state: " + ($r4 | Where-Object { $_ -match "^states=" })
$summary += "opengl fps: " + ($r1 | Where-Object { $_ -match "^timed_frames" })
$summary += "nojit fps: " + ($r3 | Where-Object { $_ -match "^timed_frames" })
if ($r5) { $summary += "software fps: " + ($r5 | Where-Object { $_ -match "^timed_frames" }) }
$summary += "cross-process state load: " + (($r6 | Where-Object { $_ -match "^(load_file|hash frame=)" }) -join " | ")
$summary | Out-File -Encoding utf8 (Join-Path $OutDir "summary.txt")
Write-Host "`n==== summary ===="
$summary | ForEach-Object { Write-Host $_ }
