param(
    # Build the zlob backend instead of the default pure-Rust ripgrep walker.
    # Needs zig 0.16.0 on PATH and libclang.dll for bindgen.
    [switch]$Zlob
)

$ErrorActionPreference = "Stop"

if ($Zlob) {
    # scoop's llvm manifest exports LIBCLANG_PATH at user scope; this only covers
    # shells that started before that variable existed.
    if (-not $env:LIBCLANG_PATH) {
        $env:LIBCLANG_PATH = 'D:\program\scoop\apps\llvm\current\bin'
    }
    cargo build --release -p fff-mcp --no-default-features --features zlob @args
}
else {
    cargo build --release -p fff-mcp @args
}
