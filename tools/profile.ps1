param(
    [Parameter(Mandatory = $true)][int]$Id,
    [int]$Samples = 15,
    [int]$IntervalMs = 1000,
    [int]$HotThreads = 3,
    [string]$SymPath = "",
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

$cdb = Find-Cdb
$proc = Get-Process -Id $Id
$stem = [IO.Path]::GetFileNameWithoutExtension($proc.Path)
$exeDir = Split-Path $proc.Path
$module = $stem -replace '-', '_'

$searchDirs = @($exeDir) + ($SymPath -split ';' | Where-Object { $_ })
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
    }
}

$finalCpu = Get-ThreadCpu (Get-Process -Id $Id -ErrorAction SilentlyContinue)
Write-Host ''
Write-Host 'cpu burned per thread while sampling'
$finalCpu.Keys | ForEach-Object {
    $tid = $_
    if ($firstCpu.ContainsKey($tid)) {
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

if ($sawNeighbourSymbol) {
    Write-Host ''
    Write-Warning 'offsets over 0x10000 mean the pdb has no symbol for that code.'
    Write-Warning 'rebuild with: cargo build --profile prof -p fff-mcp-http, then -SymPath target\prof'
}
Write-Host ''
Write-Host "raw logs: $OutDir"
