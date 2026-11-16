param(
    [Parameter(Mandatory = $true)][int]$Id,
    [ValidateSet('stacks', 'timeline')][string]$Mode = 'stacks',
    [int]$Samples = 15,
    [int]$IntervalMs = 1000,
    [int]$HotThreads = 3,
    [string]$SymPath = "",
    [string]$LogFile = "",
    [string]$OutDir = "$env:TEMP\fff-profile"
)

$ErrorActionPreference = 'Stop'

function Find-Cdb {
    $candidates = @(
        'C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe',
        'C:\Program Files\Windows Kits\10\Debuggers\x64\cdb.exe'
    )
    foreach ($path in $candidates) {
        if (Test-Path $path) { return $path }
    }
    $found = Get-Command cdb.exe -ErrorAction SilentlyContinue
    if ($found) { return $found.Source }
    throw 'cdb.exe not found: install the Windows SDK "Debugging Tools for Windows"'
}

function Get-ThreadCpu {
    param([System.Diagnostics.Process]$Process)
    $map = @{}
    foreach ($thread in $Process.Threads) {
        try { $map[[int]$thread.Id] = $thread.TotalProcessorTime.TotalSeconds } catch {}
    }
    return $map
}

# The server writes one file per run: %LOCALAPPDATA%\fff\<stem>+<ts>+<pid>.log
function Find-LogFile {
    param([int]$Id)
    $dir = Join-Path $env:LOCALAPPDATA 'fff'
    if (-not (Test-Path $dir)) { return '' }
    $hit = Get-ChildItem $dir -Filter "*+$Id.log" -File -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($hit) { return $hit.FullName }
    return ''
}

function ConvertTo-Millis {
    param([double]$Value, [string]$Unit)
    if ($Unit -eq 's') { return $Value * 1000 }
    if ($Unit -eq 'ms') { return $Value }
    if ($Unit -eq 'ns') { return $Value / 1000000 }
    return $Value / 1000
}

function Get-LogEvent {
    param([string]$Line)
    # A span logs once when it opens and once when it closes; only the close carries the cost.
    if ($Line -match 'close time\.busy=' -and $Line -match 'grep_search\{file_count=(\d+)\}') {
        return "grep($($Matches[1])f)"
    }
    if ($Line -match 'SCAN: Walk completed in [\d.]+ms \((\d+) files') { return "walk($($Matches[1])f)" }
    if ($Line -match 'parent scan subsumed') { return 'subsume' }
    if ($Line -match 'snapshot hit rate root=') { return 'snapshot' }
    if ($Line -match 'Spawning background threads') { return 'scan-request' }
    return ''
}

$cdb = if ($Mode -eq 'stacks') { Find-Cdb } else { '' }
$proc = Get-Process -Id $Id
$stem = [IO.Path]::GetFileNameWithoutExtension($proc.Path)
$exeDir = Split-Path $proc.Path
$module = $stem -replace '-', '_'
if (-not $LogFile) { $LogFile = Find-LogFile -Id $Id }

if ($Mode -eq 'timeline') {
    $span = $IntervalMs / 1000.0
    $startUtc = [DateTime]::UtcNow
    $firstCpu = Get-ThreadCpu $proc
    $prevProcess = $proc.TotalProcessorTime.TotalSeconds
    $marks = @()

    Write-Host ("timeline pid {0} ({1}) {2} samples @ {3}ms" -f $Id, $stem, $Samples, $IntervalMs)
    if ($LogFile) { Write-Host "log: $LogFile" } else { Write-Warning 'no per-run log found; CPU only' }

    for ($i = 1; $i -le $Samples; $i++) {
        Start-Sleep -Milliseconds $IntervalMs
        $now = Get-Process -Id $Id -ErrorAction SilentlyContinue
        if (-not $now) { Write-Warning 'process exited'; break }
        $cpu = $now.TotalProcessorTime.TotalSeconds
        $marks += [pscustomobject]@{ Second = [math]::Round($i * $span, 1); Cpu = $cpu - $prevProcess }
        $prevProcess = $cpu
    }

    $wall = [math]::Round($marks.Count * $span, 1)
    $lastProc = Get-Process -Id $Id -ErrorAction SilentlyContinue
    $lastCpu = if ($lastProc) { Get-ThreadCpu $lastProc } else { @{} }
    $events = @()
    if ($LogFile -and (Test-Path $LogFile)) {
        foreach ($line in (Get-Content $LogFile)) {
            if ($line -notmatch '^(\S+Z)\s') { continue }
            $stamp = [DateTime]::Parse($Matches[1], [Globalization.CultureInfo]::InvariantCulture,
                [Globalization.DateTimeStyles]::RoundtripKind).ToUniversalTime()
            $offset = ($stamp - $startUtc).TotalSeconds
            if ($offset -lt 0 -or $offset -ge $wall) { continue }
            $label = Get-LogEvent $line
            if ($label) { $events += [pscustomobject]@{ Bucket = [math]::Floor($offset / $span) + 1; Label = $label } }
        }
    }

    Write-Host ''
    Write-Host 'cpu seconds per interval, with the log events of that same interval'
    foreach ($mark in $marks) {
        $cores = if ($span -gt 0) { $mark.Cpu / $span } else { 0 }
        $tags = $events | Where-Object { $_.Bucket -eq ([math]::Round($mark.Second / $span)) } |
            Group-Object Label | ForEach-Object {
                if ($_.Count -gt 1) { "$($_.Name)x$($_.Count)" } else { $_.Name }
            }
        $bar = if ($cores -gt 0) { '#' * [math]::Min(48, [int][math]::Round($cores * 6)) } else { '' }
        '  {0,6}s {1,6:N2}s {2,-48}  {3}' -f $mark.Second, $mark.Cpu, $bar, ($tags -join ', ')
    }

    $total = ($marks | Measure-Object Cpu -Sum).Sum
    $busy = @($marks | Where-Object { $_.Cpu -gt ($span / 2) })
    Write-Host ''
    'total {0:N2}s cpu over {1}s wall = {2:N1}% of one core; {3} of {4} intervals above half a core' -f `
        $total, $wall, ($total / [math]::Max($wall, 0.001) * 100), $busy.Count, $marks.Count

    Write-Host ''
    Write-Host 'cpu burned per thread while sampling'
    $lastCpu.Keys | ForEach-Object {
        if ($firstCpu.ContainsKey($_)) {
            [pscustomobject]@{ Tid = $_; Seconds = [math]::Round($lastCpu[$_] - $firstCpu[$_], 2) }
        }
    } | Sort-Object Seconds -Descending | Select-Object -First 10 | Format-Table -AutoSize

    if ($LogFile -and (Test-Path $LogFile)) {
        $spans = @{}
        foreach ($line in (Get-Content $LogFile)) {
            if ($line -notmatch '^(\S+Z)\s') { continue }
            $stamp = [DateTime]::Parse($Matches[1], [Globalization.CultureInfo]::InvariantCulture,
                [Globalization.DateTimeStyles]::RoundtripKind).ToUniversalTime()
            if (($stamp - $startUtc).TotalSeconds -lt 0) { continue }
            if (-not ($line -match '([\w:]+)\s*: close time\.busy=([0-9.]+)(µs|μs|us|ms|s|ns)')) { continue }
            $name = $Matches[1]
            $ms = ConvertTo-Millis ([double]$Matches[2]) $Matches[3]
            if (-not $spans.ContainsKey($name)) { $spans[$name] = @{ N = 0; Max = 0.0; Sum = 0.0 } }
            $entry = $spans[$name]
            $entry.N++
            $entry.Sum += $ms
            if ($ms -gt $entry.Max) { $entry.Max = $ms }
        }
        Write-Host 'spans that closed in this window (time.busy)'
        $spans.GetEnumerator() | ForEach-Object {
            [pscustomobject]@{
                Span = $_.Key
                Calls = $_.Value.N
                TotalMs = [math]::Round($_.Value.Sum, 1)
                MaxMs = [math]::Round($_.Value.Max, 1)
            }
        } | Sort-Object TotalMs -Descending | Format-Table -AutoSize
        Write-Host 'fff_search::log / rmcp::service are lifetime spans: their busy is wall clock, not CPU.'
    }
    return
}

$searchDirs = @($exeDir) +
    @(
        (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release'),
        (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\prof')
    ) +
    ($SymPath -split ';' | Where-Object { $_ })
$pdb = $null
foreach ($dir in $searchDirs) {
    foreach ($name in @("$module.pdb", "$stem.pdb")) {
        $candidate = Join-Path $dir $name
        if (Test-Path $candidate) { $pdb = $candidate; break }
    }
    if ($pdb) { break }
}

if ($pdb) {
    Write-Host "symbols: $pdb"
} else {
    Write-Warning "no $module.pdb next to the exe; pass -SymPath <dir holding it>"
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$scriptPath = Join-Path $OutDir 'sample.cdb'
$logPath = Join-Path $OutDir 'samples.log'
Remove-Item $logPath -ErrorAction SilentlyContinue
Set-Content -Path $scriptPath -Encoding utf8 -Value @(
    ".sympath $($searchDirs -join ';')"
    '.symopt+ 0x40'
    ".reload /f $stem.exe"
    '~*k 12'
    'q'
)

$firstCpu = Get-ThreadCpu $proc
$prevCpu = $firstCpu
$names = @{}
$frameHits = @{}
$frameSample = @{}
$sawNeighbourSymbol = $false
$sawRayonSpin = $false

Write-Host ("sampling pid {0} ({1}) x{2} @ {3}ms" -f $Id, $stem, $Samples, $IntervalMs)

for ($i = 1; $i -le $Samples; $i++) {
    Start-Sleep -Milliseconds $IntervalMs
    $proc = Get-Process -Id $Id -ErrorAction SilentlyContinue
    if (-not $proc) { Write-Warning 'process exited'; break }

    $nowCpu = Get-ThreadCpu $proc
    $hot = @{}
    $nowCpu.Keys | ForEach-Object {
        if ($prevCpu.ContainsKey($_)) {
            $delta = $nowCpu[$_] - $prevCpu[$_]
            if ($delta -gt 0.002) { $hot[$_] = $delta }
        }
    }
    $prevCpu = $nowCpu
    if ($hot.Count -eq 0) { continue }
    $hotTids = @($hot.GetEnumerator() | Sort-Object Value -Descending |
        Select-Object -First $HotThreads | ForEach-Object { $_.Key })

    $sampleLog = Join-Path $OutDir ("sample-{0:d3}.log" -f $i)
    & $cdb -pv -p $Id -cf $scriptPath -logo $sampleLog | Out-Null

    $lines = Get-Content $sampleLog -ErrorAction SilentlyContinue
    if (-not $lines) { continue }
    Add-Content -Path $logPath -Value $lines

    $currentTid = 0
    $seen = @{}
    foreach ($line in $lines) {
        if ($line -match 'Id:\s*[0-9a-f]+\.([0-9a-f]+)') {
            $currentTid = [Convert]::ToInt32($matches[1], 16)
            if ($line -match '"([^"]*)"') { $names[$currentTid] = $matches[1] }
            continue
        }
        if ($line -notmatch '!') { continue }
        if ($line -match '\(Inline Function\)') { continue }
        if (-not ($hotTids -contains $currentTid)) { continue }
        if ($line -match 'ntdll!|KERNELBASE!|KERNEL32!|ucrtbase!|VCRUNTIME') { continue }
        if ($seen.ContainsKey($currentTid)) { continue }
        $seen[$currentTid] = $true
        $site = ($line -split '\s+')[-1]
        if (-not $frameHits.ContainsKey($site)) { $frameHits[$site] = 0; $frameSample[$site] = $i }
        $frameHits[$site]++
        if ($site -match '\+0x([0-9a-f]{5,})') { $sawNeighbourSymbol = $true }
        if ($site -match 'rayon_core::sleep') { $sawRayonSpin = $true }
    }
}

$finalCpu = Get-ThreadCpu (Get-Process -Id $Id -ErrorAction SilentlyContinue)
$threadTotal = 0.0
Write-Host ''
Write-Host 'cpu burned per thread while sampling'
$finalCpu.Keys | ForEach-Object {
    $tid = $_
    if ($firstCpu.ContainsKey($tid)) {
        $threadTotal += $finalCpu[$tid] - $firstCpu[$tid]
        [pscustomobject]@{
            Tid = $tid
            Seconds = [math]::Round($finalCpu[$tid] - $firstCpu[$tid], 2)
            Name = if ($names.ContainsKey($tid)) { $names[$tid] } else { '' }
        }
    }
} | Sort-Object Seconds -Descending | Select-Object -First 10 | Format-Table -AutoSize

Write-Host 'top frames (first project frame per hot thread per round)'
$frameHits.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First 20 | ForEach-Object {
    Write-Host ("  {0,3}  {1}" -f $_.Value, $_.Key)
}

if ($sawRayonSpin) {
    Write-Host ''
    Write-Warning 'a rayon_core::sleep frame is the pool going cold: real CPU, but no work behind it.'
    Write-Warning 'spread across every fff-search-N thread it can dominate a short sample.'
}

if ($sawNeighbourSymbol) {
    Write-Host ''
    Write-Warning 'offsets over 0x10000 mean this pdb has no symbol for that code.'
    Write-Warning 'python build_fff_mcp_http.py keeps debuginfo; plain cargo build --release strips it.'
    Write-Warning 'or: cargo build --profile prof -p fff-mcp-http, then -SymPath target\prof'
}
Write-Host ''
Write-Host "raw logs: $OutDir"
