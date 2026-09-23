param(
  [switch]$rebase,
  [switch]$merge
)

$ErrorActionPreference = "Stop"
# 保证 git 输出（UTF-8）能被 PowerShell 正确解码，避免中文提交信息被误当 GBK 产生乱码
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

# 运行 git 并在失败时中止：原生命令的非零退出码不会触发 $ErrorActionPreference，
# 必须显式检查 $LASTEXITCODE，否则 rebase 冲突后仍会继续 force push。
function Invoke-Git {
  $output = & git @args 2>&1
  if ($LASTEXITCODE -ne 0) {
    throw "git $($args -join ' ') failed (exit $LASTEXITCODE)"
  }
  $output
}

$script:dir = Split-Path $PSCommandPath -Parent
Push-Location $script:dir

try {
  # 确保本地在 main
  $branch = Invoke-Git branch --show-current
  if ($branch -ne "main") {
    Invoke-Git checkout main
  }

  # 丢弃本地未提交修改
  Invoke-Git reset --hard
  Write-Host "Reset local changes" -ForegroundColor Cyan

  # 拉取上游最新（origin = dmtrKovalenko/fff）
  Write-Host "Fetching origin main..." -ForegroundColor Cyan
  Invoke-Git fetch origin main

  if ($merge) {
    Write-Host "Merging origin/main..." -ForegroundColor Yellow
    Invoke-Git merge origin/main --no-edit
    Write-Host "Pushing to fork..." -ForegroundColor Cyan
    Invoke-Git push doraemoncyx main
  } else {
    Write-Host "Squashing branch commits into one..." -ForegroundColor Yellow
    $mergeBase = Invoke-Git merge-base HEAD origin/main
    $head = Invoke-Git rev-parse HEAD
    if ($mergeBase -ne $head) {
      $msg = Invoke-Git log -1 --format=%s
      Invoke-Git reset --soft $mergeBase
      Invoke-Git commit -m $msg
    }
    Write-Host "Rebasing onto origin/main..." -ForegroundColor Yellow
    Invoke-Git rebase origin/main
    Write-Host "Force pushing to fork..." -ForegroundColor Cyan
    Invoke-Git push doraemoncyx main --force-with-lease
  }
} finally {
  Pop-Location
}
