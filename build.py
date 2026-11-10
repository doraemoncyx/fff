#!/usr/bin/env python3
"""Build fff-mcp (release) with zlob feature; extra args are passed to cargo."""

from __future__ import annotations

import glob
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

LLVM_PATHS = [
    r"C:\Program Files\LLVM\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin",
]

# zlob 1.8.0 declares .minimum_zig_version = "0.17.1"
MIN_ZIG = (0, 17, 1)
ROOT = Path(__file__).resolve().parent
ZIG_GLOBS = [
    str(ROOT / "tools" / "zig-*" / "zig.exe"),
    r"D:\program\zig*\zig.exe",
    r"C:\cache\tools\zig-*\zig.exe",
]


def setup_libclang() -> None:
    for p in LLVM_PATHS:
        if (Path(p) / "libclang.dll").exists():
            os.environ["LIBCLANG_PATH"] = p
            return
    print(
        "warning: libclang.dll not found; zlob build may fail. Set LIBCLANG_PATH manually.",
        file=sys.stderr,
    )


def parse_zig_version(path: str) -> tuple[int, ...] | None:
    try:
        out = subprocess.run([path, "version"], capture_output=True, text=True, timeout=60)
    except OSError:
        return None
    if out.returncode != 0:
        return None
    m = re.match(r"(\d+)\.(\d+)\.(\d+)", out.stdout.strip())
    return tuple(int(g) for g in m.groups()) if m else None


def setup_zig() -> None:
    if os.environ.get("ZIG"):
        return
    candidates = []
    for pattern in ZIG_GLOBS:
        candidates += glob.glob(pattern)
    path_zig = shutil.which("zig")
    if path_zig:
        candidates.append(path_zig)
    for candidate in candidates:
        version = parse_zig_version(candidate)
        if version and version >= MIN_ZIG:
            os.environ["ZIG"] = candidate
            os.environ["PATH"] = str(Path(candidate).parent) + os.pathsep + os.environ["PATH"]
            print(f"using zig {'.'.join(map(str, version))}: {candidate}")
            return
    print(
        f"warning: zig >= {'.'.join(map(str, MIN_ZIG))} not found (zlob 1.8.0 needs it); "
        "set ZIG to a zig.exe path.",
        file=sys.stderr,
    )


def main() -> int:
    setup_libclang()
    setup_zig()
    return subprocess.run(
        ["cargo", "build", "--release", "-p", "fff-mcp", "--features", "zlob", *sys.argv[1:]]
    ).returncode


if __name__ == "__main__":
    sys.exit(main())
