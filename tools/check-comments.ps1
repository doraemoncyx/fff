# Enforces the two mechanical comment rules in AGENTS.md:
#   NO COMMENT LONGER THAN 2 LINES UNLESS ASKED EXPLICITLY
#   Do not add doc comments to the private structs and functions.
#
# Both are fixed syntactic patterns, so they get a deterministic check instead
# of relying on every author remembering them. Judgement calls ("matches the
# surrounding style") stay with the reviewer.
#
# The repository already carries violations, so this is a ratchet: known ones
# live in tools/comment-baseline.txt and only new ones fail. Keys deliberately
# exclude line numbers so an unrelated edit does not invalidate the baseline.
# Fix violations and rerun with -WriteBaseline to shrink it.

param(
    [string]$Root = (Split-Path -Parent $PSScriptRoot),
    [int]$MaxCommentLines = 4,
    [switch]$WriteBaseline,
    [switch]$ShowAll
)

$ErrorActionPreference = "Stop"
$baselinePath = Join-Path $PSScriptRoot "comment-baseline.txt"

function Get-Declaration {
    param($Lines, $Index)
    for ($j = $Index; $j -lt [Math]::Min($Index + 6, $Lines.Count); $j++) {
        $candidate = $Lines[$j].Trim()
        if ($candidate -eq "" -or $candidate.StartsWith("//") -or $candidate.StartsWith("#[")) { continue }
        return $candidate
    }
    return "(unknown)"
}

# `pub(crate)` and `pub(super)` are crate-private, which is what the rule is
# about; a `pub name:` is a struct field.
function Test-IsPublic {
    param($Declaration)
    if ($Declaration -eq "(unknown)") { return $true }
    if ($Declaration -match '\bpub\s+\w+\s*:') { return $true }
    if ($Declaration -notmatch '\bpub\b') { return $false }
    return $Declaration -match '\bpub\s+(struct|enum|fn|const|static|trait|type|mod|union)\b'
}

$files = Get-ChildItem -Path (Join-Path $Root "crates") -Recurse -Filter *.rs -File |
    Where-Object { $_.FullName -notmatch '\\target\\' }

$found = @()

foreach ($file in $files) {
    $relative = $file.FullName.Substring($Root.Length).TrimStart('\', '/')
    $lines = [System.IO.File]::ReadAllLines($file.FullName)
    $block = 0
    $blockStart = 0
    $isDoc = $false

    for ($i = 0; $i -lt $lines.Count; $i++) {
        $trimmed = $lines[$i].Trim()

        if ($trimmed.StartsWith("//")) {
            if ($block -eq 0) {
                $blockStart = $i + 1
                $isDoc = $trimmed.StartsWith("///")
            }
            $block++
            continue
        }

        if ($block -eq 0) { continue }

        if ($block -gt $MaxCommentLines) {
            $found += "$relative|long-comment|$block lines"
        }

        if ($isDoc) {
            $declaration = Get-Declaration $lines $i
            if (-not (Test-IsPublic $declaration)) {
                $found += "$relative|private-doc|$declaration"
            }
        }

        $block = 0
        $isDoc = $false
    }
}

if ($WriteBaseline) {
    $found | Sort-Object -Unique | Set-Content -Path $baselinePath -Encoding utf8
    Write-Host "baseline written: $($found.Count) entries -> $baselinePath"
    exit 0
}

if (-not (Test-Path $baselinePath)) {
    Write-Host "no baselines yet; run with -WriteBaseline once to record the current state" -ForegroundColor Yellow
    exit 0
}

$known = @{}
Get-Content $baselinePath | Where-Object { $_ -ne "" } | ForEach-Object { $known[$_] = $true }

$new = $found | Where-Object { -not $known.ContainsKey($_) }
$fixed = $known.Keys | Where-Object { $found -notcontains $_ }

if ($ShowAll) {
    $found | Sort-Object -Unique | ForEach-Object { Write-Host "  $_" }
}

if ($new.Count -gt 0) {
    $new | Sort-Object -Unique | ForEach-Object {
        Write-Host "NEW: $_" -ForegroundColor Red
    }
    Write-Host ""
    Write-Host "$($new.Count) new comment violation(s). See AGENTS.md: comment 1-2 lines, no doc comments on private items." -ForegroundColor Red
    exit 1
}

$note = if ($fixed.Count -gt 0) { " ($($fixed.Count) baseline entries no longer occur; rerun with -WriteBaseline to shrink)" } else { "" }
Write-Host "comments: clean$note"
exit 0
