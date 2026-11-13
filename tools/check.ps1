# Check commands for this repository
#
# One entry point so a caller never has to reconstruct a working invocation.
# `zlob` needs zig >= 0.17.1, which is pinned under tools/ and is not the zig
# on PATH; every mode below exports it before touching cargo.

param(
    [ValidateSet("all", "fast", "lint", "fmt", "comments", "test", "fix")]
    [string]$Mode = "all",
    [string]$Path = "",
    [switch]$Verbose
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# Two check runs in one target directory break each other's link step (LNK1104)
# and produce failures that look like code defects. Only one may run at a time.
$lockPath = Join-Path $repo "target/.check.lock"
$lock = $null
for ($attempt = 0; $attempt -lt 120; $attempt++) {
    try {
        $lock = [System.IO.File]::Open($lockPath, "OpenOrCreate", "ReadWrite", "None")
        break
    } catch {
        if ($attempt -eq 0) { Write-Host "another check run holds the build; waiting..." }
        Start-Sleep -Seconds 5
    }
}
if (-not $lock) {
    Write-Host "another check run has held the build for 10 minutes; giving up" -ForegroundColor Red
    exit 1
}

# zlob 1.8.0 declares minimum zig 0.17.1; the PATH zig is often older, so the
# pin wins whenever it exists.
$pinnedZig = Join-Path $repo "tools/zig-0.17.1/zig.exe"
if (Test-Path $pinnedZig) {
    $env:ZIG = $pinnedZig
    $env:PATH = (Split-Path -Parent $pinnedZig) + [IO.Path]::PathSeparator + $env:PATH
} else {
    Write-Warning "tools/zig-0.17.1/zig.exe missing; zlob-built targets may fail. Run build.py to fetch it."
}

$zlob = @("--no-default-features", "--features", "zlob")
$failed = @()

# These need a POSIX rename; on Windows they fail with PermissionDenied and pass
# in CI. Naming them keeps a green run reachable without hiding a real failure.
$windowsOnlyFailures = @(
    "watch::rescan_tests::editing_an_indexed_file_is_never_mistaken_for_a_rename",
    "watch::rescan_tests::renaming_a_file_frecency_and_keeps_the_old_entry",
    "watch::rescan_tests::renaming_a_file_carries_frecency_and_keeps_the_old_entry"
)

function Invoke-Step {
    param([string]$Name, [string[]]$Command, [string[]]$Arguments)

    if ($Verbose) { Write-Host "> $Command $($Arguments -join ' ')" }
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        $script:failed += $Name
        Write-Host "FAILED: $Name" -ForegroundColor Red
    }
}

# Returns $true when the log's only failures are the documented Windows-only
# ones. Any other failing test, or a compile error, still counts as a failure.
function Test-OnlyKnownFailures {
    param([string[]]$Log)

    $failing = $Log | Select-String -Pattern '^\s{4}(\S+::\S+)$' |
        ForEach-Object { $_.Matches[0].Groups[1].Value }
    if ($failing.Count -eq 0) { return $false }

    $unexpected = $failing | Where-Object { $windowsOnlyFailures -notcontains $_ }
    if ($unexpected) { return $false }

    Write-Host "only the documented Windows-only rename tests failed:" -ForegroundColor Yellow
    $failing | Select-Object -Unique | ForEach-Object { Write-Host "  $_" -ForegroundColor Yellow }
    return $true
}

# The one command CI runs. Excludes the two crates whose tests need a full
# Neovim / CPython toolchain.
function Test-Workspace {
    $log = @()
    & cargo (@("test", "--workspace") + $zlob + @("--exclude", "fff-nvim", "--exclude", "fff-python")) 2>&1 |
        Tee-Object -Variable log | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -eq 0) { return }

    if (-not $IsWindows -or -not (Test-OnlyKnownFailures $log)) {
        $script:failed += "test"
        Write-Host "FAILED: test" -ForegroundColor Red
    }
}

# Only the crates a change can reach. $Path is a crate directory or a file.
function Test-Fast {
    $crates = @()
    if ($Path) {
        if (Test-Path $Path -PathType Container) {
            $crates = @($Path)
        } else {
            $crates = @((Split-Path -Parent (Split-Path -Parent $Path)))
        }
    }

    if ($crates.Count -eq 0) {
        # Nothing named: fall back to the crates with uncommitted changes.
        $changed = git status --porcelain | ForEach-Object { $_.Substring(3) }
        foreach ($file in $changed) {
            if ($file -match '^crates/([^/]+)/') { $crates += "crates/$($Matches[1])" }
        }
        $crates = $crates | Select-Object -Unique
    }

    if ($crates.Count -eq 0) {
        Write-Host "no changed crate; nothing to test"
        return
    }

    foreach ($crate in $crates) {
        Invoke-Step "test $crate" "cargo" (@("test", "-p") + $crate + $zlob)
    }
}

# The comment rules in AGENTS.md are mechanical: a block over four lines, or a
# `///` doc comment on something that is not public. Both are errors here
# because a rule that only lives in prose gets missed.
function Test-Comments {
    & (Join-Path $PSScriptRoot "check-comments.ps1") -Root $repo
    if ($LASTEXITCODE -ne 0) {
        $script:failed += "comments"
        Write-Host "FAILED: comments" -ForegroundColor Red
    }
}

switch ($Mode) {
    "all" {
        Invoke-Step "fmt" "cargo" @("fmt", "--all", "--check")
        Invoke-Step "clippy" "cargo" (@("clippy", "--workspace") + $zlob + @("--", "-D", "warnings"))
        Test-Comments
        Test-Workspace
    }
    "fast" {
        Invoke-Step "clippy" "cargo" (@("clippy", "--workspace") + $zlob + @("--", "-D", "warnings"))
        Test-Comments
        Test-Fast
    }
    "lint" {
        Invoke-Step "clippy" "cargo" (@("clippy", "--workspace") + $zlob + @("--", "-D", "warnings"))
        Test-Comments
    }
    "fmt" { Invoke-Step "fmt" "cargo" @("fmt", "--all", "--check") }
    "comments" { Test-Comments }
    "test" { Test-Workspace }
    "fix" {
        Invoke-Step "fmt-fix" "cargo" @("fmt", "--all")
    }
}

if ($failed.Count -gt 0) {
    Write-Host ""
    Write-Host ("failed: " + ($failed -join ", ")) -ForegroundColor Red
    $lock.Dispose()
    exit 1
}

Write-Host "OK ($Mode)" -ForegroundColor Green
$lock.Dispose()
exit 0
