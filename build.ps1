$ErrorActionPreference = "Stop"

$llvmPaths = @(
    "C:\Program Files\LLVM\bin",
    "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin",
    "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin",
    "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin"
)

$clangFound = $false
foreach ($p in $llvmPaths) {
    if (Test-Path "$p\libclang.dll") {
        $env:LIBCLANG_PATH = $p
        $clangFound = $true
        break
    }
}

if (-not $clangFound) {
    Write-Warning "libclang.dll not found. zlob build may fail. Set LIBCLANG_PATH manually."
}

cargo build --release --features zlob @args
