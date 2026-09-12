<#
.SYNOPSIS
    Batch-converts old Super Shuckie replays (format v2/v3) to format v4 in place, verifying each one.

.DESCRIPTION
    Walks every "*-data\replays\*.replay" under -Root, skips files that are already v4, and for the
    rest runs supershuckie-replay-convert with --verify into a temporary file next to the original.
    Only when the verify passes is the original moved to -Backup (or deleted with -DeleteOriginals)
    and the converted file renamed into its place. A failed conversion leaves the original untouched
    and keeps a log next to it.

    Up to -Parallel conversions run at once; each needs roughly as much RAM as its input file and
    one CPU core. Files written in the last -MinAgeMinutes minutes are skipped (an in-progress
    recording must not be converted).

.EXAMPLE
    .\convert-replays.ps1 -DryRun
    .\convert-replays.ps1 -Parallel 4
    .\convert-replays.ps1 -Root "D:\replays" -Backup "E:\replay-backup" -Level 19

.NOTES
    If the replays live in a Dropbox folder, pause Dropbox syncing for the duration: it would
    otherwise re-upload every converted file while the batch runs and may briefly lock files.
#>
[CmdletBinding()]
param(
    # Folder that contains the "<ROM>-data\replays" folders (the app's UserData directory).
    [string]$Root = "A:\Dropbox\stp-projects\programs\supershuckie 2\UserData",

    # The converter binary (cargo build --release -p supershuckie-replay-recorder --features convert).
    [string]$Converter = (Join-Path $PSScriptRoot "target\release\supershuckie-replay-convert.exe"),

    # Where verified originals are moved to (mirroring their path under -Root). Ignored with -DeleteOriginals.
    [string]$Backup = "A:\replay-backup-v3",

    # Delete verified originals instead of moving them to -Backup.
    [switch]$DeleteOriginals,

    # Number of conversions to run at once.
    [int]$Parallel = 4,

    # zstd level for the output (9 = default; 19 is ~10% smaller at roughly twice the conversion time).
    [int]$Level = 9,

    # Keep every keyframe bit-exact (no transient-buffer masks); files come out ~1.5-2x larger.
    [switch]$NoMasks,

    # Only list what would be converted.
    [switch]$DryRun,

    # Skip files modified more recently than this (a recording in progress).
    [int]$MinAgeMinutes = 10
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Converter)) {
    throw "Converter not found at $Converter - build it with: cargo build --release -p supershuckie-replay-recorder --features convert"
}
if (-not (Test-Path $Root)) {
    throw "Root folder not found: $Root"
}

function Get-ReplayVersion([string]$path) {
    # Header: 4-byte signature "NIDO" (0x4E49444F big-endian), then the u32 LE format version.
    $stream = [System.IO.File]::OpenRead($path)
    try {
        $header = New-Object byte[] 8
        $read = $stream.Read($header, 0, 8)
        if ($read -lt 8) { return $null }
        if ($header[0] -ne 0x4E -or $header[1] -ne 0x49 -or $header[2] -ne 0x44 -or $header[3] -ne 0x4F) { return $null }
        return [BitConverter]::ToUInt32($header, 4)
    }
    finally {
        $stream.Close()
    }
}

function Format-Size([double]$bytes) {
    $units = "B", "KiB", "MiB", "GiB", "TiB"
    $unit = 0
    while ($bytes -ge 1024 -and $unit -lt $units.Length - 1) { $bytes /= 1024; $unit++ }
    return ("{0:N1} {1}" -f $bytes, $units[$unit])
}

function Move-WithRetry([string]$from, [string]$to) {
    # Dropbox and antivirus scanners briefly hold freshly written files; retry a few times.
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        try {
            Move-Item -LiteralPath $from -Destination $to -Force
            return
        }
        catch {
            if ($attempt -eq 5) { throw }
            Start-Sleep -Seconds (2 * $attempt)
        }
    }
}

# ---- Collect the work ---------------------------------------------------------------------------

$cutoff = (Get-Date).AddMinutes(-$MinAgeMinutes)
$candidates = @()
$skipped = @{ "already v4" = 0; "not a replay" = 0; "too recent" = 0; "temp file" = 0 }

$replayDirs = Get-ChildItem -LiteralPath $Root -Directory -Filter "*-data" | ForEach-Object {
    $replays = Join-Path $_.FullName "replays"
    if (Test-Path -LiteralPath $replays) { Get-Item -LiteralPath $replays }
}

foreach ($dir in $replayDirs) {
    foreach ($file in Get-ChildItem -LiteralPath $dir.FullName -File -Filter "*.replay") {
        if ($file.Name -like "*.v4-tmp.replay") {
            # Leftover from an interrupted run; the original is still there, so just drop it.
            if (-not $DryRun) { Remove-Item -LiteralPath $file.FullName -Force }
            $skipped["temp file"]++
            continue
        }
        if ($file.LastWriteTime -gt $cutoff) { $skipped["too recent"]++; continue }
        $version = Get-ReplayVersion $file.FullName
        if ($null -eq $version) { $skipped["not a replay"]++; continue }
        if ($version -ge 4) { $skipped["already v4"]++; continue }
        $candidates += $file
    }
}

# Biggest first: packs the parallel workers best.
$candidates = $candidates | Sort-Object Length -Descending
$totalBytes = ($candidates | Measure-Object Length -Sum).Sum
if ($null -eq $totalBytes) { $totalBytes = 0 }

Write-Host ("{0} replays to convert ({1}); skipped: {2} already v4, {3} too recent, {4} not replays, {5} temp files" -f `
    $candidates.Count, (Format-Size $totalBytes), $skipped["already v4"], $skipped["too recent"], $skipped["not a replay"], $skipped["temp file"])

if ($DryRun) {
    foreach ($file in $candidates) {
        Write-Host ("  {0,10}  v{1}  {2}" -f (Format-Size $file.Length), (Get-ReplayVersion $file.FullName), $file.FullName.Substring($Root.Length).TrimStart("\"))
    }
    return
}
if ($candidates.Count -eq 0) { return }

# ---- Run the conversions ------------------------------------------------------------------------

$started = Get-Date
$running = @()
$done = 0
$failed = @()
$bytesBefore = 0.0
$bytesAfter = 0.0

function Start-Conversion($file) {
    $tmp = [System.IO.Path]::ChangeExtension($file.FullName, ".v4-tmp.replay")
    $log = $file.FullName + ".convert.log"
    if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force }

    $arguments = @(('"{0}"' -f $file.FullName), ('"{0}"' -f $tmp), "--verify", "--level", $Level)
    if ($NoMasks) { $arguments += "--no-masks" }

    $process = Start-Process -FilePath $Converter -ArgumentList $arguments -NoNewWindow -PassThru -RedirectStandardError $log
    # Windows PowerShell only reports ExitCode for processes whose handle was touched before they exited.
    $null = $process.Handle
    return [PSCustomObject]@{ File = $file; Tmp = $tmp; Log = $log; Process = $process; Started = Get-Date }
}

function Complete-Conversion($job) {
    $file = $job.File
    $relative = $file.FullName.Substring($Root.Length).TrimStart("\")
    $seconds = [int]((Get-Date) - $job.Started).TotalSeconds

    if ($job.Process.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $job.Tmp)) {
        if (Test-Path -LiteralPath $job.Tmp) { Remove-Item -LiteralPath $job.Tmp -Force }
        $script:failed += $relative
        Write-Host ("FAILED  {0} (exit {1}, {2}s) - see {3}" -f $relative, $job.Process.ExitCode, $seconds, $job.Log) -ForegroundColor Red
        return
    }

    $newSize = (Get-Item -LiteralPath $job.Tmp).Length
    if ($DeleteOriginals) {
        Remove-Item -LiteralPath $file.FullName -Force
    }
    else {
        $destination = Join-Path $Backup $relative
        New-Item -ItemType Directory -Force -Path (Split-Path $destination) | Out-Null
        Move-WithRetry $file.FullName $destination
    }
    Move-WithRetry $job.Tmp $file.FullName
    Remove-Item -LiteralPath $job.Log -Force

    $script:done++
    $script:bytesBefore += $file.Length
    $script:bytesAfter += $newSize
    Write-Host ("ok      {0}: {1} -> {2} ({3:N1}x) in {4}s   [{5}/{6}]" -f $relative, (Format-Size $file.Length), (Format-Size $newSize), ($file.Length / [Math]::Max($newSize, 1)), $seconds, ($script:done + $script:failed.Count), $candidates.Count)
}

$queue = [System.Collections.Generic.Queue[object]]::new()
foreach ($file in $candidates) { $queue.Enqueue($file) }

try {
    while ($queue.Count -gt 0 -or $running.Count -gt 0) {
        while ($queue.Count -gt 0 -and $running.Count -lt $Parallel) {
            $running += Start-Conversion $queue.Dequeue()
        }

        Start-Sleep -Seconds 2

        $finished = @($running | Where-Object { $_.Process.HasExited })
        foreach ($job in $finished) { Complete-Conversion $job }
        $running = @($running | Where-Object { -not $_.Process.HasExited })
    }
}
finally {
    foreach ($job in $running) {
        if (-not $job.Process.HasExited) { $job.Process.Kill() }
        if (Test-Path -LiteralPath $job.Tmp) { Remove-Item -LiteralPath $job.Tmp -Force -ErrorAction SilentlyContinue }
    }
}

$elapsed = (Get-Date) - $started
Write-Host ""
Write-Host ("converted {0} of {1} replays in {2:hh\:mm\:ss}: {3} -> {4}" -f $done, $candidates.Count, $elapsed, (Format-Size $bytesBefore), (Format-Size $bytesAfter))
if (-not $DeleteOriginals -and $done -gt 0) { Write-Host ("originals moved to {0}" -f $Backup) }
if ($failed.Count -gt 0) {
    Write-Host ("{0} FAILED (originals untouched, logs kept next to them):" -f $failed.Count) -ForegroundColor Red
    $failed | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    exit 1
}
